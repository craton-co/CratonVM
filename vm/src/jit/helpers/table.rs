use cratonvm_native_api::NativeCallSiteExt;
#[allow(unused_imports)]
use super::*;
// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

/// Return the current thread's `JvmThread` pointer for the JIT inline
/// TLAB bump-pointer fast path.
///
/// Reads the same TLS slot (`JIT_THREAD`) populated by `set_jit_thread`
/// just before JIT-compiled code runs. The pointer is valid for the
/// duration of the JIT invocation and is cleared by `clear_jit_thread`
/// when JIT code returns.
///
/// Returning a raw pointer is intentional — the JIT immediately reads
/// the TLAB cursor/end fields from `[thread + tlab_offset + ..]` and
/// never dereferences anything outside that two-word window during the
/// fast path. The slow-path fallback (`jit_new_object`) reaches the
/// thread via the same TLS slot.
///
/// Returns `null` if invoked from a thread that did not call
/// `set_jit_thread` (defensive — the JIT fast path treats a null thread
/// pointer as "skip the inline bump, fall through to slow path").
// SAFETY: takes no pointer arguments; only reads the per-thread `JIT_THREAD` TLS
// slot and returns it (null if `set_jit_thread` was never called). Safe to call
// from JIT-compiled code.
#[no_mangle]
pub unsafe extern "C" fn jit_get_current_thread() -> *mut JvmThread {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    JIT_THREAD.with(|t| t.get())
}

// ===========================================================================
// BUG-1 companion — native-stack headroom guard for DIRECT self-recursive
// calls.
//
// The dispatch helpers contain the thread-local depth guard (`enter_jit_
// dispatch`) that converts runaway compiled recursion into a catchable
// `StackOverflowError`. Routing every non-tail self-recursive `invokestatic`
// through `jit_invoke_dispatch` purely to reach that guard made each
// recursive call pay the full dispatch round trip (scan-cache invalidation,
// SATB flush, dispatch-cache lookup, borrow bookkeeping, …) — the dominant
// cost of recursive workloads (fib(42) ran ~10x slower than with a direct
// CALL; binarytrees pays it on both `make` and `check`).
//
// This guard is the cheap replacement the backend calls immediately before
// a direct rel32 self-CALL: compare the current native stack pointer against
// a per-thread floor (queried from the OS once per thread and cached in
// TLS). Comfortably above the floor → return 0 and the compiled site
// proceeds with the direct CALL. At exhaustion → stash a catchable
// `java/lang/StackOverflowError` (identical to the dispatch depth guard) and
// return the `i64::MIN` deopt sentinel, which the call site routes through
// its existing post-invoke sentinel check.
// ===========================================================================
thread_local! {
    /// Cached self-call guard floor for the current thread.
    /// `usize::MAX` = not yet computed.
    pub static JIT_SELF_CALL_STACK_FLOOR: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// Query the current thread's native stack bounds `(low, high)` from the OS.
/// `None` when the platform query is unavailable/fails — the caller falls
/// back to a conservative offset from the first observed stack pointer.
#[cfg(windows)]
pub fn thread_stack_bounds() -> Option<(usize, usize)> {
    extern "system" {
        fn GetCurrentThreadStackLimits(low: *mut usize, high: *mut usize);
    }
    let mut low = 0usize;
    let mut high = 0usize;
    // SAFETY: plain out-pointer Win32 call on the current thread.
    unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
    if low != 0 && high > low {
        Some((low, high))
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
pub fn thread_stack_bounds() -> Option<(usize, usize)> {
    // SAFETY: standard pthread attr query on the current thread; attr is
    // initialized by pthread_getattr_np on success and destroyed after use.
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return None;
        }
        let mut addr: *mut libc::c_void = std::ptr::null_mut();
        let mut size: libc::size_t = 0;
        let rc = libc::pthread_attr_getstack(&mut attr, &mut addr, &mut size);
        libc::pthread_attr_destroy(&mut attr);
        if rc != 0 || addr.is_null() || size == 0 {
            return None;
        }
        Some((addr as usize, addr as usize + size))
    }
}

#[cfg(target_os = "macos")]
pub fn thread_stack_bounds() -> Option<(usize, usize)> {
    // SAFETY: both are infallible pthread queries on the current thread.
    unsafe {
        let top = libc::pthread_get_stackaddr_np(libc::pthread_self()) as usize;
        let size = libc::pthread_get_stacksize_np(libc::pthread_self());
        if top == 0 || size == 0 {
            return None;
        }
        Some((top - size, top))
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn thread_stack_bounds() -> Option<(usize, usize)> {
    None
}

/// How much stack one thread may burn on COMPILED recursion before the guard
/// converts it into a catchable `StackOverflowError` — the JIT-side analogue of
/// `-Xss`, and the compiled counterpart of the interpreter's
/// `JvmConfig::max_stack_depth` frame cap.
///
/// Without this bound the two tiers disagree by more than an order of
/// magnitude on WHEN a program overflows, because they measure different
/// things: the interpreter counts frames in a heap `Vec` and stops at
/// `max_stack_depth` (8192 by default), while compiled frames live on the
/// native stack and were allowed to consume all of it. Measured on this host
/// with an 8 MiB carrier stack, one trivially recursive method reached:
///
/// | tier | depth before StackOverflowError |
/// |---|---|
/// | CratonVM interpreter (`--nojit`) | 8 191 |
/// | CratonVM JIT (before this bound) | 232 417 |
/// | HotSpot JDK 25 (interpreted → C2) | 11 820 → 23 306 |
///
/// A 28x tier-dependent answer is a fidelity bug on its own — the depth at
/// which a program overflows should not hinge on whether a method happened to
/// get compiled — and HotSpot's own two numbers differ by only ~2x because
/// both of ITS tiers are bounded by the same `-Xss`.
///
/// It also breaks real tests. `io.netty.util.concurrent.DefaultPromiseTest`
/// sizes its work from the depth it measures (`stackOverflowDepth << 1`), then
/// gives that work a fixed 2-second deadline; at 232k the chain was ~465 000
/// promises where HotSpot builds ~24 000–47 000, so
/// `testNoStackOverflowWithDefaultEventExecutorA/B` timed out with the JIT on
/// and passed with `--nojit`. It was never a promise or notification defect:
/// CratonVM completes HotSpot's own chain lengths in 343 ms / 798 ms against
/// that 2 s budget.
///
/// 4 MiB, set by measurement rather than by analogy. A byte budget does not
/// translate to a fixed depth — that depends on the compiled frame size, which
/// is exactly how HotSpot behaves too — so the number is chosen to land the
/// depth ABOVE the interpreter's own 8192-frame cap and inside HotSpot's range.
/// On the probe above, 4 MiB gives ~10k-21k frames against HotSpot's
/// 11.8k-23.3k.
///
/// 1 MiB (HotSpot's default `-Xss`) was tried first and is WRONG here: it
/// produced 2434-5305 frames, i.e. STRICTER than the interpreter, which only
/// re-opens the tier gap from the other side and could reject recursion that
/// runs fine under `--nojit`. CratonVM's compiled frames are simply larger than
/// HotSpot's, so matching HotSpot's byte budget does not match its depth.
///
/// 4 MiB is also what the no-OS-bounds fallback below has always assumed, so
/// the two paths now agree instead of differing by the whole stack size.
pub const SELF_CALL_STACK_BUDGET: usize = 4 << 20; // 4 MiB

/// JIT round 23 wave 6, lane L3 (`CRATONVM_JIT_SELF_CALL_BUDGET_HOTSPOT_DEPTH`;
/// pages `r23-w4-compiled-recursion-shallower-than-interpreter`,
/// `r23-w2-lambda-recursion-overflows-at-380-levels`): the budget sized for
/// HotSpot's DEPTH, as a fixed ratio of the thread's Java stack size.
///
/// HotSpot 25 reaches `H4b` `plain=` 24 931 and `lamref=` 41 550 on its
/// default 1 MiB stack: 25-42 B per level, because C2 inlines the recursion
/// (`MaxRecursiveInlineLevel`) and the lambda, and its frames are a few words.
/// CratonVM's compiled levels measure 484 B (optimizing `rec`, 480 B frame),
/// 633 B (single-pass `recLam` through the direct lambda arm, 592 B frame)
/// and 1 108 B (`lam`: the lambda body's frame plus `recLam2`'s) on `n23-w5`,
/// about 12-20x HotSpot's per level, so the flat 4 MiB gave 8 665 / 6 626 /
/// 3 785. The budget is therefore `16 x` the thread's Java stack size
/// (`-Xss`, or a `Thread` constructor's `stackSize`; HotSpot's 1 MiB default
/// when neither is given), which reproduces HotSpot's depth scaling with
/// `-Xss`: 16 MiB by default (about 34 000 / 26 000 / 15 000 levels against
/// 24 931 / 41 550 / 41 550), 4 MiB at `-Xss256k`, 256 MiB at `-Xss16m`.
///
/// Safe on every thread: the floor is still never below the thread's own
/// guard floor (`compute_self_call_stack_floor_in`), so a budget larger than
/// the carrier stack is bounded by it (the main thread runs on 128 MiB, a
/// grown carrier on the request plus 8 MiB). The StackOverflowError stays
/// catchable (the same trip and unwind; measured with a 20 MiB budget).
/// `=0` restores the flat [`SELF_CALL_STACK_BUDGET`] plus the carrier growth.
pub const SELF_CALL_BUDGET_PER_JAVA_STACK_BYTE: usize = 16;

/// HotSpot's default thread stack size on Windows and Linux x64 (1 MiB): the
/// Java stack size of a thread with no `-Xss` and no `stackSize` request.
pub const HOTSPOT_DEFAULT_JAVA_STACK_BYTES: usize = 1 << 20;

/// The per-thread inputs of the compiled-recursion budget, copied out of the
/// [`JvmThread`] (per VM: the thread's own recorded request, from `VmConfig`'s
/// `-Xss` or its constructor's `stackSize`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SelfCallBudget {
    /// [`JvmThread::self_call_extra_budget`]: the carrier growth (flat mode).
    pub extra: usize,
    /// [`JvmThread::self_call_java_stack_bytes`]: the thread's Java stack
    /// size, `0` for HotSpot's default.
    pub java_stack: usize,
}

impl SelfCallBudget {
    /// The budget inputs `thread` recorded.
    pub fn of_thread(thread: &JvmThread) -> Self {
        Self {
            extra: thread.self_call_extra_budget,
            java_stack: thread.self_call_java_stack_bytes,
        }
    }

    /// The byte budget below the first observed stack pointer. Read on the
    /// cold floor computations only.
    pub fn bytes(self) -> usize {
        if true {
            let java_stack = if self.java_stack == 0 {
                HOTSPOT_DEFAULT_JAVA_STACK_BYTES
            } else {
                self.java_stack
            };
            return java_stack.saturating_mul(SELF_CALL_BUDGET_PER_JAVA_STACK_BYTE);
        }
        // gcd d6/f + JIT round 13: a thread whose carrier `thread_start` grew
        // for a `stackSize` / `-Xss` request gets the growth on top of the
        // flat budget (`JvmThread::self_call_extra_budget`, 0 for every other
        // thread). `CRATONVM_JIT_SELF_CALL_BUDGET_FOLLOWS_STACK_SIZE=0` keeps
        // the flat budget.
        let extra = if self.extra != 0
            && true {
            self.extra        } else {
            0        };
        SELF_CALL_STACK_BUDGET.saturating_add(extra)    }}


/// Compute the guard floor for this thread: the lowest stack pointer at which
/// a compiled self-recursive site may still CALL one level deeper.
///
/// The headroom below the floor must cover everything that can run once the
/// guard trips: the `StackOverflowError` construction (class load + alloc),
/// the compiled frames' unwind back through the sentinel checks, and any
/// interpreter/exception-table routing above. 1 MiB is generous for all of
/// those; on small stacks [`self_call_floor_margin`] shrinks it, but never
/// below the optimizing tier's sampling stride allows.
///
/// The result is then raised to at most [`SELF_CALL_STACK_BUDGET`] below the
/// first observed SP — see that constant for why the whole-stack floor is the
/// wrong bound. Taking the HIGHER (tighter) of the two keeps the guard-page
/// headroom guarantee intact: this only ever trips EARLIER, never later.
#[cold]
pub fn compute_self_call_stack_floor(sp_now: usize, budget: SelfCallBudget) -> usize {
    compute_self_call_stack_floor_in(sp_now, budget, thread_stack_bounds())
}

/// [`compute_self_call_stack_floor`] for a thread whose stack bounds the
/// caller already holds (`None`: unknown). Interpreter round i3 wave 79, lane
/// L3: the reflective doors pass the bounds they cache, so a rebase asks the
/// OS nothing ([`rebase_self_call_stack_floor`]).
#[cold]
pub fn compute_self_call_stack_floor_in(
    sp_now: usize,
    budget: SelfCallBudget,
    bounds: Option<(usize, usize)>,
) -> usize {
    let guard_floor = match bounds {
        Some((low, high)) => {
            let size = high - low;
            low.saturating_add(self_call_floor_margin(
                size,
                true,
            ))
        }
        // No OS query available: assume at least ~4 MiB of stack below the
        // first observed SP (threads here default to 8 MiB). This still
        // converts unbounded recursion into a catchable error well before
        // a typical guard page.
        None => sp_now.saturating_sub(4 << 20),
    };
    // The byte budget (`SelfCallBudget::bytes`): 16 x the Java stack size,
    // or the flat budget plus the carrier growth with the switch off.
    guard_floor.max(sp_now.saturating_sub(budget.bytes()))
}

/// The installed thread's [`SelfCallBudget`] inputs, or the defaults when no
/// thread is installed on this OS thread. Read through the raw pointer without
/// forming a reference: an outer JIT level may hold the thread's `&mut`.
pub fn current_self_call_budget() -> SelfCallBudget {
    JIT_THREAD.with(|t| {
        let p = t.get();
        if p.is_null() {
            SelfCallBudget::default()
        } else {
            // SAFETY: `JIT_THREAD` is non-null only while `set_jit_thread`'s
            // scope keeps the thread alive; two plain `usize` reads.
            unsafe {
                SelfCallBudget {
                    extra: std::ptr::addr_of!((*p).self_call_extra_budget).read(),
                    java_stack: std::ptr::addr_of!((*p).self_call_java_stack_bytes).read(),
                }
            }
        }
    })
}

/// The optimizing tier samples the stack once per 64 KiB boundary
/// (`ir_lower::emit_self_recursive_call`: `MOVZX EAX, SP; CMP EAX, span`), so
/// a trip can land up to one stride plus one frame BELOW the floor.
pub const SELF_CALL_SAMPLE_STRIDE: usize = 64 * 1024;

/// Bytes reserved between the stack's low end and the self-call guard floor.
///
/// The legacy margin, a quarter of the stack clamped to 64 KiB..1 MiB, is
/// exactly one sample stride on a 256 KiB stack: the sample that crosses the
/// floor's boundary passes, the next one sits at `low` itself, and the
/// prologue's stack bang reaches the OS guard page before any check trips
/// (round 12 wave 2 hunter, `self-call-guard-stride-exceeds-small-stack-margin`).
/// With `stride_margin` the margin is at least three strides, so a sampled trip
/// still leaves a stride for building and unwinding the `StackOverflowError`,
/// capped at half the stack so a small stack keeps usable room above the floor.
/// Stacks of 4 MiB and more are unchanged (their margin is the full 1 MiB).
pub fn self_call_floor_margin(size: usize, stride_margin: bool) -> usize {
    const HEADROOM: usize = 1 << 20; // 1 MiB
    let legacy = HEADROOM.min(size / 4).max(64 * 1024);
    if stride_margin {
        legacy.max((3 * SELF_CALL_SAMPLE_STRIDE).min(size / 2))
    } else {
        legacy
    }
}


/// Round 13 wave 4 (lane mega3, M13-3): this OS thread's compiled-recursion
/// floor, get-or-computed exactly as [`jit_native_stack_floor`] does, for
/// `set_jit_thread` to copy into `JvmThread::jit_stack_floor`. Computing it
/// there anchors the 4 MiB budget ([`SELF_CALL_STACK_BUDGET`]) at the first
/// Rust-to-compiled entry of the thread, never deeper than the first compiled
/// query used to, so the floor can only be tighter (trip earlier), never
/// looser.
#[inline]
pub fn current_thread_self_call_floor(budget: SelfCallBudget) -> usize {
    let probe = 0u8;
    let sp_now = &probe as *const u8 as usize;
    JIT_SELF_CALL_STACK_FLOOR.with(|f| {
        let v = f.get();
        if v != usize::MAX {
            v
        } else {
            let computed = compute_self_call_stack_floor(sp_now, budget);
            f.set(computed);
            computed
        }
    })
}



/// Leaf floor query for the INLINE self-recursion check: get-or-compute the
/// current OS thread's native-stack floor (the same TLS value
/// `jit_self_call_stack_guard` consults). Called ONCE from the prologue of a
/// method with direct self-recursive call sites; each site then compares RSP
/// against the frame-cached value inline. Touches no VM state and never GCs
/// (no scan-cache boundary note needed — a leaf like `jit_get_current_thread`).
///
/// SAFETY: no arguments, reads only this thread's TLS + stack bounds.
#[no_mangle]
pub unsafe extern "C" fn jit_native_stack_floor() -> i64 {
    let probe = 0u8;
    let sp_now = &probe as *const u8 as usize;
    JIT_SELF_CALL_STACK_FLOOR.with(|f| {
        let v = f.get();
        if v != usize::MAX {
            v
        } else {
            let computed = compute_self_call_stack_floor(sp_now, current_self_call_budget());
            f.set(computed);
            computed
        }
    }) as i64
}

/// The self-call stack guard baked before every direct self-recursive CALL.
/// Returns `0` (proceed) or the `i64::MIN` deopt sentinel with a catchable
/// `java/lang/StackOverflowError` stashed in `JIT_PENDING_EXCEPTION`.
///
// SAFETY: called from JIT-compiled code; `vm_ptr` is the SharedVm pointer the
// compiled frame received at entry (same contract as `jit_invoke_dispatch`).
#[no_mangle]
pub unsafe extern "C" fn jit_self_call_stack_guard(vm_ptr: i64, ctor_receiver: i64) -> i64 {
    let f = || jit_self_call_stack_guard_body(vm_ptr, ctor_receiver);
    contain(
        "jit_self_call_stack_guard",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_self_call_stack_guard`], run under its panic guard.
// SAFETY: Called under `contain` panic guard with JIT caller arguments.
pub unsafe fn jit_self_call_stack_guard_body(vm_ptr: i64, ctor_receiver: i64) -> i64 {
    // Rust<->JIT boundary — invalidate the per-thread JIT-scan cache (same
    // single TLS bump `jit_get_current_thread` performs on the inline-TLAB
    // fast path).
    crate::jit::conservative_roots::note_jit_boundary();
    let probe = 0u8;
    let sp_now = &probe as *const u8 as usize;
    let floor = JIT_SELF_CALL_STACK_FLOOR.with(|f| {
        let v = f.get();
        if v != usize::MAX {
            v
        } else {
            let computed = compute_self_call_stack_floor(sp_now, current_self_call_budget());
            f.set(computed);
            computed
        }
    });
    if sp_now > floor {
        return 0;
    }
    // SAFETY: vm_ptr originates from JIT code and points to the live SharedVm.
    let vm = &*(vm_ptr as *const SharedVm);
    crate::runtime::interpreter::withdraw_compiled_constructor_at_entry(vm, ctor_receiver);
    let rc = raise_jit_stack_overflow(vm);
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] self-call stack guard TRIP sp={:#x} floor={:#x} pending={}",
            sp_now,
            floor,
            jit_pending_exception_is_set(),
        );
    }
    rc
}

/// spring-bug-10 watchpoint: arm a hardware data WRITE breakpoint (DR0) on the
/// savebase frame slot at `addr` for the CURRENT thread, so the vectored
/// exception handler can report the RIP that writes the corrupt `0xFFFF…FFFE`.
///
/// Called from the JIT prologue (under `CRATONVM_SHADOW_WATCH`) with
/// `addr = rbp - savebase_off`. Re-arms only when `addr` changes (reset is
/// invoked repeatedly at the same stack depth ⇒ usually a no-op) and stops once
/// the VEH has caught the write. Setting debug registers on the running current
/// thread via `SetThreadContext(GetCurrentThread())` is applied by the kernel on
/// the next ring transition.
/// True if `ra` (a JIT return address) resolves to the no-arg `Matcher.reset()`.
/// Requires `CRATONVM_DBG_JIT_NAMES=1` (populates the name registry).
#[cfg(windows)]
pub fn ra_is_reset(ra: usize) -> bool {
    match cratonvm_jit::lookup_jit_method_name(ra) {
        Some(n) => n.contains("Matcher.reset()"),
        None => false,
    }
}

/// spring-bug-10 watchpoint via a dedicated WATCHER THREAD. Setting debug
/// registers on the *running current* thread is unreliable (the DRs only reload
/// on a context-switch-IN, which races the write we want to trap). The fix: a
/// background thread that SUSPENDS the worker, `SetThreadContext`s its DR0/DR7,
/// and RESUMES — applied deterministically on resume. The worker's reset prologue
/// just publishes its current savebase address; the watcher keeps DR0 pinned to
/// it (reset is called at a stable stack depth, so this is steady-state idle).
#[cfg(windows)]
pub mod savebase_watcher {
    use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};

    pub static ARM_ADDR: AtomicUsize = AtomicUsize::new(0);
    pub static WORKER_HANDLE: AtomicIsize = AtomicIsize::new(0);
    pub static STARTED: AtomicBool = AtomicBool::new(false);

    extern "system" {
        // Returns the Win32 pseudo-HANDLE as `*mut c_void` to stay structurally
        // identical to the `GetCurrentProcess` decl in
        // `runtime::crash_handler::windows_fault` — same symbol, so
        // `clashing_extern_declarations` compares the two and warns if they
        // diverge. The handle is pointer-sized either way; this module treats
        // handles as `isize` (see `DuplicateHandle` below), so the single call
        // site casts the result with `as isize`.
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn GetCurrentThread() -> isize;
        fn DuplicateHandle(
            sp: isize,
            sh: isize,
            tp: isize,
            th: *mut isize,
            access: u32,
            inherit: i32,
            opts: u32,
        ) -> i32;
        fn SuspendThread(t: isize) -> u32;
        fn ResumeThread(t: isize) -> u32;
        fn SetThreadContext(t: isize, ctx: *const u8) -> i32;
    }
    const DUPLICATE_SAME_ACCESS: u32 = 0x2;

    // SAFETY: `h` must be a valid, suspended thread HANDLE owned by this watcher;
    // the CONTEXT is a correctly aligned 1232-byte buffer and the offsets written
    // (ContextFlags/Dr0/Dr7) match the Win32 x64 CONTEXT layout passed to
    // SetThreadContext.
    unsafe fn set_dr_on(h: isize, addr: u64, dr7: u64) -> i32 {
        #[repr(C, align(16))]
        struct Ctx([u8; 1232]);
        let mut c = Ctx([0u8; 1232]);
        let p = c.0.as_mut_ptr();
        core::ptr::write_unaligned(p.add(0x30) as *mut u32, 0x0010_0010); // DEBUG_REGISTERS
        core::ptr::write_unaligned(p.add(0x48) as *mut u64, addr); // Dr0
        core::ptr::write_unaligned(p.add(0x70) as *mut u64, dr7); // Dr7
        SetThreadContext(h, p)
    }

    /// Worker-side: publish the current reset savebase address; start the watcher
    /// thread on first call (duplicating the worker's thread handle for it).
    // SAFETY: called on the worker thread; the Win32 handle-duplication and thread
    // spawn use only valid pseudo-handles (GetCurrentProcess/GetCurrentThread) and
    // store the duplicated real handle for the watcher to Suspend/Resume.
    pub unsafe fn publish(addr: usize) {
        ARM_ADDR.store(addr, Ordering::Relaxed);
        if STARTED.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut h: isize = 0;
        let proc = GetCurrentProcess() as isize;
        DuplicateHandle(
            proc,
            GetCurrentThread(),
            proc,
            &mut h,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        );
        WORKER_HANDLE.store(h, Ordering::SeqCst);
        eprintln!(
            "[WATCH] watcher thread started; worker savebase @0x{:016X}",
            addr
        );
        // SAFETY: `watcher_loop` only Suspend/Resume/SetThreadContext's the
        // duplicated worker HANDLE stored in `WORKER_HANDLE`; no shared Rust state
        // is aliased mutably across threads (all coordination is via atomics).
        std::thread::spawn(|| unsafe { watcher_loop() });
    }

    // SAFETY: runs on the dedicated watcher thread; the only handle it touches is
    // the duplicated worker HANDLE in `WORKER_HANDLE` (valid until process exit),
    // and the Suspend→SetThreadContext→Resume sequence keeps the worker quiesced
    // while its debug registers are written.
    unsafe fn watcher_loop() {
        let mut set_addr = 0usize;
        loop {
            let h = WORKER_HANDLE.load(Ordering::SeqCst);
            if crate::runtime::crash_handler::savebase_watch_caught() {
                if h != 0 {
                    SuspendThread(h);
                    set_dr_on(h, 0, 0);
                    ResumeThread(h);
                }
                return;
            }
            let want = ARM_ADDR.load(Ordering::Relaxed);
            if h != 0 && want != 0 && want != set_addr {
                let susp = SuspendThread(h);
                // DR7: L0 + R/W0=01 (write) + LEN0=10 (8-byte).
                let stc = set_dr_on(h, want as u64, 0x0009_0001);
                let res = ResumeThread(h);
                if set_addr == 0 {
                    eprintln!(
                        "[WATCH] watcher armed DR0=0x{:016X} on worker h=0x{:X}: SuspendThread={} SetThreadContext={} ResumeThread={}",
                        want, h, susp as i32, stc, res as i32
                    );
                }
                set_addr = want;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

// Naked trampoline: read `[rsp]` (the true return address into reset's prologue)
// at entry, tail-jump to the inner handler with it in RDX (ARG1).
// SAFETY: naked fn — its body is hand-written asm that reads the on-stack return
// address and tail-jumps to `arm_savebase_watch_inner` with the extern "C" ABI
// preserved (`addr` in RCX/ARG0, the return address placed in RDX/ARG1).
#[cfg(windows)]
#[unsafe(naked)]
pub unsafe extern "C" fn jit_arm_savebase_watch(addr: i64) {
    core::arch::naked_asm!(
        "mov rdx, [rsp]",
        "jmp {inner}",
        inner = sym arm_savebase_watch_inner,
    );
}

// SAFETY: called only from the `jit_arm_savebase_watch` naked trampoline with the
// extern "C" ABI it sets up; `ra` is the JIT return address read off the stack and
// `addr` is a savebase slot address that is validated (non-null, 8-byte aligned)
// before use.
#[cfg(windows)]
pub unsafe extern "C" fn arm_savebase_watch_inner(addr: i64, ra: usize) {
    if crate::runtime::crash_handler::savebase_watch_caught() {
        return;
    }
    if !ra_is_reset(ra) {
        return;
    }
    let a = addr as usize;
    if a == 0 || a & 0x7 != 0 {
        return;
    }
    savebase_watcher::publish(a);
}

// Disarm is watcher-managed (it disarms on catch), so the epilogue helper is a
// no-op; the cross-frame filter in the VEH separates the live corruptor from a
// coincidental -2 write to the reused stack slot after reset returns.
// SAFETY: empty body — takes no arguments and dereferences nothing.
#[cfg(windows)]
pub unsafe extern "C" fn jit_disarm_savebase_watch() {}

// SAFETY: non-Windows stub with an empty body — takes no pointer it dereferences.
#[cfg(not(windows))]
pub unsafe extern "C" fn jit_arm_savebase_watch(_addr: i64) {}
// SAFETY: non-Windows stub with an empty body — dereferences nothing.
#[cfg(not(windows))]
pub unsafe extern "C" fn jit_disarm_savebase_watch() {}

/// Build the JIT runtime helpers table with real function pointer addresses.
/// Build the helper table for a SPECIFIC VM.
///
/// Three of the fields below are VM-specific addresses: the STW flag the
/// JIT polls, and the card-table triple its inline reference store marks
/// into. They used to be resolved through `process_vm()` — the FIRST VM
/// published in the process — so in a second VM the compiled code would poll
/// another VM's safepoint flag and write card marks into another VM's
/// table. A missed card mark is a missed remembered-set update, which is a
/// use-after-free, not a slowdown. See
/// `vm-process-global-state.md`.
///
/// Every production caller has its own `SharedVm` in scope and should use
/// this. [`build_helpers`] remains for VM-less unit tests.
pub fn build_helpers_for(shared: &crate::vm::SharedVm) -> JitRuntimeHelpers {
    build_helpers_opt(Some(shared))
}

/// Build the helper table from the process-global VM, if one is published.
///
/// Prefer [`build_helpers_for`] wherever a `SharedVm` is in scope: this
/// spelling silently picks the FIRST VM in the process.
pub fn build_helpers() -> JitRuntimeHelpers {
    build_helpers_opt(crate::native::jni::process_vm().as_deref())
}

pub fn ref_store_plan_for_heap(
    heap: Option<&crate::memory::vm_heap::VmHeap>,
) -> ((usize, usize, usize), usize) {
    match heap {
        Some(heap) if heap.is_generational() => {
            let (pre, post, floor, mask) = heap.gen_jit_ref_store_plan().unwrap_or((0, 0, 0, 0));
            ((pre, post, floor), usize::from(mask))
        }
        Some(heap) if heap.is_zgc() => {
            let (pre, post, floor, mask) = heap.zgc_jit_ref_store_plan().unwrap_or((0, 0, 0, 0));
            ((pre, post, floor), usize::from(mask))
        }
        // g1put: G1 publishes no PLAN (no post gate, no floor, no mask), so
        // `ref_store_gates_of` still answers `None` and no generational-style
        // gated arm reaches it. But its SATB queue arms the process-wide pre
        // byte, which the optimizing tier's G1 store reads to keep an
        // overwrite of a non-null reference inline while no mark is running
        // (`ir_lower::emit_g1_ir_ref_putfield`).
        Some(heap) if heap.is_g1() => {
            ((cratonvm_gc::jit_satb_pre_gate_addr(), 0, 0), 0)
        }
        _ => (
            cratonvm_gc::jit_ref_store_gate_addrs(),
            usize::from(cratonvm_gc::jit_ref_store_post_skip_mask()),
        ),
    }
}

/// JIT round 21 wave 1, lane updaters (review UPD21-R2): do the plan words of
/// [`ref_store_plan_for_heap`], `((pre, post, young_floor), post_skip_mask)`,
/// make the JIT's gated reference store available (`x64::ref_store_gates_of`
/// then `ir_lower::vh_ref_store_gates`)? Both gate bytes published and exactly
/// one post shape (a young floor XOR a skip mask). G1 publishes `(pre, 0, 0)`
/// and a heap that has not published yet `(0, 0, 0)`: neither does. Pure.
pub fn ref_store_plan_words_publish_gates(plan: ((usize, usize, usize), usize)) -> bool {
    let ((pre, post, floor), mask) = plan;
    pre != 0 && post != 0 && ((floor != 0) ^ (mask != 0))
}

pub fn build_helpers_opt(vm_for_helpers: Option<&crate::vm::SharedVm>) -> JitRuntimeHelpers {
    // Compute the inline-TLAB offset triple once at startup so the JIT
    // can bake them as immediates. The runtime tests
    // `Tlab::test_tlab_offsets` and `JvmThread::tlab_offset_matches_field_address`
    // pin the layout against drift.
    let tlab_off = JvmThread::tlab_offset();
    let cursor_in_thread = tlab_off + cratonvm_gc::Tlab::CURSOR_OFFSET;
    let end_in_thread = tlab_off + cratonvm_gc::Tlab::END_OFFSET;

    // The savebase watchpoint hooks and the compile-time static-slot resolver
    // travel on each compile: `direct_helper_table` / `direct_helper_table_for`.

    // §7/§10 — the execution policy is NOT published to the JIT here.
    //
    // This spot used to call `cratonvm_jit::set_jit_execution_policy`, which
    // latched a process-global `AtomicU8` with `fetch_max`: the first
    // `--real-jdk` VM in a process made every later VM compile over-strict,
    // and nothing could lower it. Every compile-time consumer of the policy
    // now takes it as an argument from a site that holds this VM —
    // `dispatch_policy(shared).is_jdk_only()` at the compile doors in
    // `jit_bridge.rs`, and the `jit_entry_publishable` publication sites below
    // — so the policy a compilation sees is always its own VM's, fixed for the
    // VM's life. The latch and its setter were deleted on 2026-09-12; see
    // `jit-compatibility-and-despec-state-per-vm-FIXED.md`.

    // Registered UNCONDITIONALLY as of 2026-08-06 (JDK-ONLY-WAVE2 §2). These
    // are process-invariant Rust `fn` addresses — the same code whatever a
    // given VM's policy is — so withholding them was never per-VM protection,
    // it was a process-wide side effect: whichever VM built helpers under
    // `JdkOnly` left the cells at `0` for every other VM in the process, and
    // once the latch went strict `direct_native_helper` refused for all of
    // them too. Binding is now decided per compilation from the threaded
    // policy, which is the only place that knows whose compile it is.
    {
        // §4 census, declared at the wiring point rather than paid per call.
        // Every helper wired immediately above reaches its native without a
        // `NativeMethodId`, so `record_invocation` never fires for it and those
        // rows' `invocations` are floors — see
        // [`DIRECT_CALL_HELPER_NATIVES`] for the measured evidence and
        // [`mark_direct_call_helper_natives_incomplete`] for why the bit goes
        // here instead of a `+9.2 ns/call` counter inside bodies that exist to
        // avoid a 141 ns boundary. A VM-less `build_helpers()` has no registry
        // to tell, and `--nojit` never reaches this function at all, which is
        // exactly right: those runs' counts really are exact.
        if let Some(shared) = vm_for_helpers {
            mark_direct_call_helper_natives_incomplete(shared);
        }
    }

    // gen r4w4/cards4 (2026-09-24): fetched again, and now free.
    //
    // `jit_card_table_info()` returns the card table's READ-ONLY view
    // (`cratonvm_gc::card_table::JitCardView`), not the raw card map. Compiled
    // code only READS card bytes through it and calls `jit_write_barrier` for
    // every clean→dirty transition, so fetching it no longer disarms the scan
    // bound or the summary map the way `CardTable::jit_cards_addr` does
    // (`gengc-oldgen-card-scan-is-o-heap-FIXED-20260923.md`). Two readers:
    // the inline barrier behind `CRATONVM_JIT_INLINE_CARD_MARK` (default off),
    // and the `aastore` arms, which read the view's precise-array flag to pass
    // the ELEMENT's address to the barrier. `None` (G1, ZGC, a VM-less unit
    // build) leaves all three at zero, which every reader takes as "no
    // generational card metadata".
    //
    // Per VM (`vm_for_helpers`), never `process_vm()`: the July 2026 emitter
    // baked the FIRST VM's triple into every VM's code (fixed 2026-08-01 by
    // `build_helpers_for`), which in a second VM is a barrier that range-tests
    // against another heap and skips every card.
    let (jit_card_table_addr, jit_card_old_base, jit_card_old_end) = vm_for_helpers
        .and_then(|shared| shared.mem.heap.jit_card_table_info())
        .unwrap_or((0, 0, 0));

    // `(pre, post, young_floor)` — all-zero until a collector publishes a
    // reference-store barrier plan. Read once, so the three fields below can
    // never name three different generations of the table.
    //
    // gen r4w4/cards4: a generational heap's plan comes from THAT heap
    // (`GenerationalHeap::jit_ref_store_plan`), retaining the per-heap isolation
    // introduced by the generational cards repair
    // (`gengc-r4-cards-jit-post-barrier-gate-is-process-global-FIXED-20260924.md`).
    // ZGC also owns both gates. G1 emits no store plan; only a VM-less
    // unit build reads the legacy process table.
    let (ref_store_gates, ref_store_post_skip_mask) =
        ref_store_plan_for_heap(vm_for_helpers.map(|s| &s.mem.heap));

    let helpers = JitRuntimeHelpers {
        newarray: jit_newarray as *const () as usize,
        new_object: jit_new_object as *const () as usize,
        anewarray_object: jit_anewarray_object as *const () as usize,
        // Cold-`new` fix: the CP-indexed variants, used for a `new`/`anewarray`
        // whose target class was not loaded when the method compiled. Always
        // wired in production — the OptionalPtr classification exists only so
        // hand-built test tables can leave them 0.
        new_object_cp: jit_new_object_cp as *const () as usize,
        anewarray_object_cp: jit_anewarray_object_cp as *const () as usize,
        // Same story for `ldc <Class>`: the mirror is a heap object and the
        // target may not be loaded at compile time, so the site is served by a
        // CP-indexed helper. Always wired in production.
        ldc_class_cp: jit_ldc_class_cp as *const () as usize,
        // JVMS §6.5 covariance check only — the `0x53` lowering keeps its
        // inline store, its SATB barrier and its card mark and calls out
        // only for this. `required: true` in `jit-api`, so a `0` here is a
        // `validate()` failure rather than a silently unchecked store.
        aastore_type_check: jit_aastore_type_check as *const () as usize,
        // These two existed but were unreachable from the JIT: correct
        // implementations with no table slot, so `ir_lower` had nothing to call
        // and monitors could not be lowered at all.
        monitor_enter: jit_monitor_enter as *const () as usize,
        monitor_exit: jit_monitor_exit as *const () as usize,
        baload: jit_baload as *const () as usize,
        bastore: jit_bastore as *const () as usize,
        iaload: jit_iaload as *const () as usize,
        iastore: jit_iastore as *const () as usize,
        aaload: jit_aaload as *const () as usize,
        aastore: jit_aastore as *const () as usize,
        multianewarray_2d: jit_multianewarray_2d as *const () as usize,
        multianewarray_n: jit_multianewarray_n as *const () as usize,
        arraylength: jit_arraylength as *const () as usize,
        getfield: jit_getfield as *const () as usize,
        putfield_int: jit_putfield_int as *const () as usize,
        putfield_long: jit_putfield_long as *const () as usize,
        putfield_float: jit_putfield_float as *const () as usize,
        putfield_double: jit_putfield_double as *const () as usize,
        putfield_object: jit_putfield_object as *const () as usize,
        getstatic: jit_getstatic as *const () as usize,
        putstatic_int: jit_putstatic_int as *const () as usize,
        putstatic_long: jit_putstatic_long as *const () as usize,
        putstatic_float: jit_putstatic_float as *const () as usize,
        putstatic_double: jit_putstatic_double as *const () as usize,
        putstatic_object: jit_putstatic_object as *const () as usize,
        checkcast: jit_checkcast as *const () as usize,
        instanceof_check: jit_instanceof as *const () as usize,
        throw_aioobe: jit_throw_aioobe as *const () as usize,
        throw_arithmetic: jit_throw_arithmetic as *const () as usize,
        invoke_dispatch: jit_invoke_dispatch as *const () as usize,
        invoke_virtual_mic: jit_invoke_virtual_mic as *const () as usize,
        service_callee_deopt: jit_service_callee_deopt as *const () as usize,
        lambda_int_to_double: jit_lambda_int_to_double as *const () as usize,
        write_barrier: jit_write_barrier as *const () as usize,
        // Round-7 fix (CRIT, UAF in JIT): SATB pre-write barrier so JIT-
        // overwritten references are logged before the concurrent marker
        // loses the only path to them.
        satb_pre_write_barrier: jit_satb_pre_write_barrier as *const () as usize,
        uncommon_trap: jit_uncommon_trap as *const () as usize,
        math_fma_double: jit_math_fma_double as *const () as usize,
        math_fma_float: jit_math_fma_float as *const () as usize,
        // HIGH-6 JIT audit — inline TLAB bump-pointer wiring.
        tlab_cursor_offset_in_thread: cursor_in_thread,
        tlab_end_offset_in_thread: end_in_thread,
        // JIT contract: `ObjectHeader.class_id` is at byte offset 0
        // (enforced by `class_id_remains_at_offset_zero` in
        // `types/src/heap_types.rs`). Exposed here so the JIT does not
        // hardcode the constant in a second place.
        class_id_offset_in_obj: 0,
        get_current_thread: jit_get_current_thread as *const () as usize,
        tlab_post_init: jit_post_tlab_init as *const () as usize,
        // ZGC inline-TLAB announce (ABI v13). Wired unconditionally: the
        // emitter calls it only when the collector requires JIT TLAB
        // registration AND the class's post-init work is otherwise a no-op,
        // and falls back to `tlab_post_init` whenever this slot is 0.
        zgc_note_tlab_object: jit_zgc_note_tlab_object as *const () as usize,
        // Stage 3 (precise oop maps) — only wire the frame-record helper when
        // the precise gate is on; otherwise leave it 0 so the prologue emits
        // nothing extra. The JIT also gates emission on its own cached flag,
        // but keying the pointer on the same env keeps the default build inert.
        frame_record: if cratonvm_jit::x64::precise_jit_maps_enabled()
            || cratonvm_jit::x64::moving_young_enabled()
        {
            // Step 1 self-check: when inline frame-record is active AND the
            // verify knob is on, wire the verify helper here instead — the
            // prologue calls it right after the inline store to assert the
            // mirror slot it wrote is the one the GC reads.
            if cratonvm_jit::x64::verify_inline_frame_record_enabled()
                && cratonvm_jit::x64::inline_rbp_tls_disp() != 0
            {
                jit_verify_inline_frame_record as *const () as usize
            } else {
                jit_frame_record as *const () as usize
            }
        } else {
            0
        },
        // Shadow-stack precise roots — byte offset of the `ShadowStack` field
        // from `&JvmThread`. The JIT bakes `[thread + this + ShadowStack::TOP_OFFSET]`
        // as the inline push target. Always wired (harmless when codegen is off,
        // which gates emission on its own cached `CRATONVM_SHADOW_STACK` flag).
        shadow_stack_offset_in_thread: JvmThread::shadow_stack_offset(),
        // Inline thin-lock `monitorenter`/`monitorexit` (ABI v15): where the
        // per-thread `JitMonitorBlock` (thin-lock owner id + JMX lock-stack
        // address) sits in `JvmThread`. 0 under `CRATONVM_MONITOR_FASTPATH=0`,
        // which keeps every compiled monitor site on its helper call.
        monitor_block_offset_in_thread:
            if crate::threading::thread_registry::monitor_fastpath_enabled() {
                JvmThread::jit_monitor_block_offset()
            } else {
                0
            },
        // Round 13 wave 4 (lane mega3, ABI v16): where the native-stack floor
        // word `set_jit_thread` keeps current sits in `JvmThread`. 0 under
        // `CRATONVM_JIT_STACK_FLOOR_WORD=0`: inline-cache stack checks keep
        // their frame-cached floor.
        jit_stack_floor_offset_in_thread: {
            std::mem::offset_of!(JvmThread, jit_stack_floor)
        },
        // RBC.6 — athrow lowering: stash pending exception + sentinel.
        throw_exception: jit_throw_exception as *const () as usize,
        set_throw_bci: jit_set_throw_bci as *const () as usize,
        // JEP 358 (helpful NPE), inline-codegen path — per-action null-check
        // failure stub target. Sets the pending NPE *with* its JEP-358 action
        // code so the interpreter drain can attach the right action-only
        // message.
        jit_npe_with_action: jit_npe_with_action as *const () as usize,
        // i64::MIN-sentinel disambiguation for J/D (long/double) call returns —
        // peeked by a compiled caller on the rare `RAX == i64::MIN` branch to
        // tell a genuine callee exception/deopt apart from a legitimate
        // `Long.MIN_VALUE` return.
        dispatch_threw: jit_dispatch_threw as *const () as usize,
        // IR FP tier (Slice A) — fmod-style FP remainder helpers, CALLed by the
        // IR `Op::Rem` Float/Double arms (operands in XMM0/XMM1, result XMM0).
        jit_frem: jit_frem as *const () as usize,
        jit_drem: jit_drem as *const () as usize,
        // BUG-1 companion — native-stack headroom guard enabling direct
        // (non-dispatch) self-recursive CALLs. See `jit_self_call_stack_guard`.
        self_call_stack_guard: jit_self_call_stack_guard as *const () as usize,
        // Guarded inline getfield — address of the GC's process-global region
        // bounds table. Non-zero even under G1/ZGC (the table just stays
        // all-zero there, so every guard falls through to the checked helper).
        region_bounds_addr: cratonvm_gc::jit_region_bounds_addr(),
        // The READ-side sibling. Same six-word shape, DIFFERENT table on
        // purpose: `JIT_REGION_BOUNDS` above is what gates inline reference
        // STORES, and G1/ZGC keep it empty so an inline store can never skip
        // `post_write_barrier_rset` (`g1-audit.md` 8.1). This one
        // answers only the read question -- is the receiver inside mapped
        // arena memory, so a raw load cannot fault -- and G1 does publish its
        // single contiguous arena span into it. ZGC publishes nothing, which
        // is what keeps inline reference reads unreachable there until the
        // ZGC JIT load barrier lands.
        read_bounds_addr: cratonvm_gc::jit_read_bounds_addr(),
        // Compiled local exception handlers. Wired unconditionally: the
        // backend only arms a stub when `local_handlers_enabled()` says so, so
        // a wired-but-unused address costs nothing, and having ONE gate (the
        // flag) rather than two (flag + wiring) is what keeps
        // "the feature reports itself on while being structurally inert" off
        // the table.
        local_handler_lookup: jit_local_handler_lookup as *const () as usize,
        // Inline self-recursion check — leaf floor-query helper (see the
        // jit-api field doc; prologue-called once per self-recursive method).
        native_stack_floor_fn: jit_native_stack_floor as *const () as usize,
        ldc_string: jit_ldc_string as *const () as usize,
        // The CP-INDEXED string `ldc`, which both backends emit instead of the
        // bytes form above since 2026-08-20. `ldc_string` stays wired because
        // the helper ABI is append-only and its slot is `RequiredPtr`; nothing
        // calls it.
        ldc_string_cp: jit_ldc_string_cp as *const () as usize,
        // FFM element accessors. Always wired in production; a hand-built test
        // table that leaves these 0 simply emits no fast path, and every site
        // keeps the native dispatch it has today.
        ffm_segment_get: jit_ffm_segment_get as *const () as usize,
        ffm_segment_set: jit_ffm_segment_set as *const () as usize,
        // Reference-store barrier gates. All three come from ONE call, so a
        // half-wired plan — two live addresses and a stale third — is not
        // expressible here: `jit_ref_store_gate_addrs` returns all-zero until a
        // collector has published, and all-zero is what every emitter arm reads
        // as "keep the full-helper path you have today".
        //
        // Deliberately NOT conditioned on the collector in this file. "May
        // compiled code skip this call" belongs to the collector that answers
        // it, and it answers by publishing or declining to; a second copy of
        // that decision here is exactly how `region_bounds_addr` came to mean
        // two different things at once.
        ref_store_pre_gate: ref_store_gates.0,
        ref_store_post_gate: ref_store_gates.1,
        ref_store_post_young_floor: ref_store_gates.2,
        // The mask shape of the same plan, as a VALUE. Read from the same
        // published table and in the same breath as the three addresses above,
        // so a plan cannot be observed half-applied; a publisher supplies
        // either this or the floor, never both.
        ref_store_post_skip_mask,
        // F-08 -- G1's inline post-write barrier: the geometry table, and the
        // call target its slow arm uses.
        //
        // A THIRD table address in this struct, and the third is not a
        // duplicate of either of the first two. `region_bounds_addr` above is
        // the table whose EMPTINESS under G1 closes defect G1-2 -- nothing may
        // ever publish into it for G1. `read_bounds_addr` answers the read-side
        // "is this address mapped". This one carries the numbers an inline G1
        // barrier needs in order to BE a barrier: arena base, arena length,
        // region mask, plus the F-05 card table's base and shift. Wired
        // unconditionally; the table itself is all-zero unless a G1 collector
        // has published, and the emitter treats a zero length as "no inline
        // barrier".
        g1_barrier_addr: cratonvm_gc::jit_g1_barrier_addr(),
        g1_post_write_barrier: jit_g1_post_write_barrier as *const () as usize,
        // Cooperative JIT safepoint polling (CRATONVM_JIT_SAFEPOINT_POLLS,
        // off by default) — address of the process-global VM's
        // stw_requested flag byte. `process_vm()` is published by
        // `Vm::new()` before any bytecode runs (see its doc comment), which
        // is always before the first `build_helpers()` call a real JIT
        // compile can trigger (compilation only starts once the
        // interpreter is executing bytecode). `None` here (e.g. a unit
        // test that calls `build_helpers()` before any `Vm::new()`) leaves
        // this at `0`, which `emit_safepoint_poll` (the SOLE reader of this
        // field) treats as "not wired" and emits no poll code at all — the
        // same optional-helper contract as `region_bounds_addr`/
        // `frame_record` above.
        safepoint_flag_addr: vm_for_helpers
            .map(|shared| shared.mem.gc_barrier.stw_requested_flag_addr() as usize)
            .unwrap_or(0),
        // Slow-path helper for a poll hit. Unconditionally wired (the
        // function always exists in this binary) — `safepoint_flag_addr`
        // above is what actually gates whether the JIT ever emits a CALL
        // to it, so leaving this non-zero when the flag address happens to
        // be unavailable is harmless (dead code, never reached).
        safepoint_slow_path: jit_safepoint_slow_path as *const () as usize,
        jit_card_table_addr,
        jit_card_old_base,
        jit_card_old_end,
    };

    // Actually run the validator this table has always shipped with.
    //
    // `JitRuntimeHelpers::validate_abi` checks the ABI revision, the struct
    // size, that every `required` slot is non-zero, and that every `Offset`
    // slot is a plausible displacement — and until now it had **no callers
    // outside `jit-api`'s own tests** (`docs/jit/helper-abi.md` §4). A
    // zeroed required slot therefore left the producer silently and surfaced
    // as a `CALL 0` from RWX memory somewhere downstream, with the crash
    // pointing at the JIT backend rather than at the one line here that failed
    // to wire a helper.
    //
    // Panicking is the correct severity: every required slot is an
    // unconditional `f as *const () as usize` of a function that exists in this
    // binary, so a failure is a build-integrity problem, not a runtime
    // condition — there is no degraded mode to fall back to, and continuing
    // means compiling machine code around a null pointer. The optional slots
    // (`safepoint_flag_addr`, `frame_record`, `region_bounds_addr`, the card
    // table triple) are legitimately zero when their mechanism is off or when
    // `process_vm()` is not yet published, and `validate_abi` does not require
    // them, so a VM-less `build_helpers()` still passes.
    //
    // The hand-built tables in `jit/tests/*` and `jit/src/x64.rs`'s
    // `test_helpers()` do NOT reach this: they construct `JitRuntimeHelpers`
    // literals directly and never call `build_helpers`, which is defined only
    // here.
    if let Err(e) = helpers.validate_abi() {
        panic!("JIT helper table is not usable: {e}");
    }
    helpers
}

/// The VM's thin direct-call helper addresses, as the table each compile
/// carries (`cratonvm_jit::DirectHelperTable`). Process-invariant Rust `fn`
/// addresses; whether a given compilation may BIND one is still decided per
/// compilation, from the threaded Real-JDK policy.
pub fn direct_helper_table() -> cratonvm_jit::DirectHelperTable {
    cratonvm_jit::DirectHelperTable {
        integer_value_of: jit_integer_value_of_direct as *const () as usize,
        identity_hash_code: jit_identity_hash_code_direct as *const () as usize,
        reflect_method_invoke_sentinel: crate::jit::helpers::jit_reflect_method_invoke_sentinel as *const () as usize,
        integer_int_value: jit_integer_int_value_direct as *const () as usize,
        long_value_of: jit_long_value_of_direct as *const () as usize,
        long_long_value: jit_long_long_value_direct as *const () as usize,
        hashmap_put: jit_hashmap_put_direct as *const () as usize,
        hashmap_get: jit_hashmap_get_direct as *const () as usize,
        ctor_new_instance: jit_ctor_new_instance_direct as *const () as usize,
        string_latin1_lower: jit_string_latin1_to_lower_direct as *const () as usize,
        concurrent_hashmap_get: jit_concurrent_hashmap_get_direct as *const () as usize,
        nio_bytebuffer_put_byte: jit_dbb_put_byte_direct as *const () as usize,
        nio_bytebuffer_get_byte: jit_dbb_get_byte_direct as *const () as usize,
        // `Buffer.session()` and the served-class table its fast path is
        // screened against — published as a pair, because the helper without
        // the predicate would answer for every receiver, which is precisely
        // the unsound version.
        buffer_session: jit_buffer_session_direct as *const () as usize,
        scoped_memory_get_int: jit_scoped_memory_get_int_direct as *const () as usize,
        scoped_memory_get_short: jit_scoped_memory_get_short_direct as *const () as usize,
        scoped_memory_get_long: jit_scoped_memory_get_long_direct as *const () as usize,
        scoped_memory_get_char: jit_scoped_memory_get_char_direct as *const () as usize,
        scoped_memory_put_int: jit_scoped_memory_put_int_direct as *const () as usize,
        scoped_memory_put_short: jit_scoped_memory_put_short_direct as *const () as usize,
        scoped_memory_put_long: jit_scoped_memory_put_long_direct as *const () as usize,
        scoped_memory_put_char: jit_scoped_memory_put_char_direct as *const () as usize,
        scoped_memory_get_byte_internal: jit_scoped_memory_get_byte_internal_direct as *const ()
            as usize,
        scoped_memory_put_byte_internal: jit_scoped_memory_put_byte_internal_direct as *const ()
            as usize,
        scoped_memory_get_int_internal: jit_scoped_memory_get_int_internal_direct as *const ()
            as usize,
        scoped_memory_get_short_internal: jit_scoped_memory_get_short_internal_direct as *const ()
            as usize,
        scoped_memory_get_long_internal: jit_scoped_memory_get_long_internal_direct as *const ()
            as usize,
        scoped_memory_get_char_internal: jit_scoped_memory_get_char_internal_direct as *const ()
            as usize,
        scoped_memory_put_int_internal: jit_scoped_memory_put_int_internal_direct as *const ()
            as usize,
        scoped_memory_put_short_internal: jit_scoped_memory_put_short_internal_direct as *const ()
            as usize,
        scoped_memory_put_long_internal: jit_scoped_memory_put_long_internal_direct as *const ()
            as usize,
        scoped_memory_put_char_internal: jit_scoped_memory_put_char_internal_direct as *const ()
            as usize,
        unsafe_get_byte: jit_unsafe_get_byte_direct as *const () as usize,
        unsafe_get_short: jit_unsafe_get_short_direct as *const () as usize,
        unsafe_get_int: jit_unsafe_get_int_direct as *const () as usize,
        unsafe_get_long: jit_unsafe_get_long_direct as *const () as usize,
        unsafe_get_reference: jit_unsafe_get_reference_direct as *const () as usize,
        unsafe_get_reference_volatile: jit_unsafe_get_reference_volatile_direct as *const ()
            as usize,
        unsafe_put_byte: jit_unsafe_put_byte_direct as *const () as usize,
        unsafe_put_short: jit_unsafe_put_short_direct as *const () as usize,
        unsafe_put_int: jit_unsafe_put_int_direct as *const () as usize,
        unsafe_put_long: jit_unsafe_put_long_direct as *const () as usize,
        unsafe_cas_int: jit_unsafe_cas_int_direct as *const () as usize,
        unsafe_cas_long: jit_unsafe_cas_long_direct as *const () as usize,
        // nio21 wave 3, lane L4.
        unsafe_get_int_volatile: jit_unsafe_get_int_volatile_direct as *const () as usize,
        unsafe_get_long_volatile: jit_unsafe_get_long_volatile_direct as *const () as usize,
        unsafe_put_int_volatile: jit_unsafe_put_int_volatile_direct as *const () as usize,
        unsafe_put_long_volatile: jit_unsafe_put_long_volatile_direct as *const () as usize,
        unsafe_put_reference_volatile: jit_unsafe_put_reference_volatile_direct as *const ()
            as usize,
        unsafe_cas_reference: jit_unsafe_cas_reference_direct as *const () as usize,
        unsafe_full_fence: jit_unsafe_full_fence_direct as *const () as usize,
        reference_refers_to0: jit_reference_refers_to0_direct as *const () as usize,
        phantom_reference_refers_to0: jit_phantom_reference_refers_to0_direct as *const () as usize,
        reference_get0: jit_reference_get0_direct as *const () as usize,
        thread_current_carrier_thread: jit_thread_current_carrier_thread_direct as *const ()
            as usize,
        // Per VM: `direct_helper_table_for` asks `unsafe_inline_admitted_mask`.
        unsafe_inline_admitted: 0,
        buffer_session_served_class: cratonvm_native_builtins::buffer_session::class_is_served
            as *const () as usize,
        // The predicate the OPTIMIZING door's bind is gated on. The JIT crate
        // does not depend on `native-io`, so it cannot ask the served-class
        // table directly; publishing the function pointer keeps the planner's
        // "will the helper serve this receiver?" and the helper's own prologue
        // reading the SAME table, which is the property that makes the bind
        // gate meaningful rather than a guess.
        nio_byte_element_served_class:
            cratonvm_native_io::direct_buffer::elem_fastpath::class_is_served as *const () as usize,
        md_update_byte: jit_md_update_byte_direct as *const () as usize,
        thread_current_thread: jit_thread_current_thread_direct as *const () as usize,
        system_current_time_millis: jit_system_current_time_millis_direct as *const () as usize,
        system_nano_time: jit_system_nano_time_direct as *const () as usize,
        object_notify: jit_object_notify_direct as *const () as usize,
        object_notify_all: jit_object_notify_all_direct as *const () as usize,
        unsafe_unpark: jit_unsafe_unpark_direct as *const () as usize,
        unsafe_park: jit_unsafe_park_direct as *const () as usize,
        thread_local_get: jit_thread_local_get_direct as *const () as usize,
        thread_local_set: jit_thread_local_set_direct as *const () as usize,
        monitor_enter: jit_monitor_enter as *const () as usize,
        monitor_exit: jit_monitor_exit as *const () as usize,
        preconditions_check_index: jit_preconditions_check_index_direct as *const () as usize,
        scoped_memory_copy_memory: jit_scoped_memory_copy_memory_direct as *const () as usize,
        unsafe_copy_memory: jit_unsafe_copy_memory_direct as *const () as usize,
        decimal_digits_latin1_int: jit_decimal_digits_latin1_int_direct as *const () as usize,
        decimal_digits_latin1_long: jit_decimal_digits_latin1_long_direct as *const () as usize,
        vectorized_hash_code: jit_vectorized_hash_code_direct as *const () as usize,
        vectorized_mismatch: jit_vectorized_mismatch_direct as *const () as usize,
        crc32_update_bytes0: jit_crc32_update_bytes0_direct as *const () as usize,
        crc32_update_bytes: jit_crc32_update_bytes_direct as *const () as usize,
        adler32_update_bytes: jit_adler32_update_bytes_direct as *const () as usize,
        byte_array_clone: jit_byte_array_clone_direct as *const () as usize,
        object_clone: jit_object_clone_direct as *const () as usize,
        // j27 wave 3 (lane CLONE): the virtual `clone()` helper and its
        // CompilerOnly one-class question (asked with the per-VM
        // `static_base_resolver_ctx` `direct_helper_table_for` adds).
        object_clone_virtual: jit_object_clone_virtual_direct as *const () as usize,
        object_clone_served_class: jit_object_clone_served_class as *const () as usize,
        sb_grow: jit_sb_grow_direct as *const () as usize,
        reachability_fence: jit_reachability_fence_direct as *const () as usize,
        // JIT round 17 wave 1, lane nio. Published unconditionally like every
        // slot here; the doors bind it only under
        // `CRATONVM_JIT_OBJECT_GET_CLASS_DIRECT=1`.
        object_get_class: jit_object_get_class_direct as *const () as usize,
        // JIT round 22 wave 2, lane runtime (RUNTIME22-2): published
        // unconditionally; the doors bind it unless
        // `CRATONVM_JIT_CLASS_GET_NAME_DIRECT=0` (default ON since R22-AB-FLIP).
        class_get_name: jit_class_get_name_direct as *const () as usize,
        class_get_superclass: jit_class_get_superclass_direct as *const () as usize,
        // Interpreter round i4 wave 92, lane L2: the deferred field site.
        // Published unconditionally; the doors defer a site only under
        // `CRATONVM_JIT_DEFERRED_FIELD_SITES` (`cp_field_deferred_resolver`).
        field_site_cp: jit_field_site_cp as *const () as usize,
        // spring-bug-10 watchpoint: the JIT prologue (under CRATONVM_SHADOW_WATCH)
        // bakes an absolute call to these.
        arm_savebase_watch: jit_arm_savebase_watch as *const () as usize,
        disarm_savebase_watch: jit_disarm_savebase_watch as *const () as usize,
        // No VM here: `direct_helper_table_for` adds the per-VM resolver, the
        // class-initialised predicate and the well-known-class resolver.
        static_base_resolver: 0,
        static_base_resolver_ctx: 0,
        class_initialized: 0,
        well_known_class_id: 0,
        // No VM here either: `direct_helper_table_for` publishes the latch
        // address under `CRATONVM_GEN_CONC_OWED_POLL`.
        gen_owed_poll_flag_addr: 0,
        // Per VM as well: `direct_helper_table_for` publishes the address of
        // the VM's compiled-`getClass()` words (nio20 L3).
        getclass_mirror_words: 0,
        zgc_read_barrier_armed_addr: 0,
        zgc_referent_gate_state_addr: 0,
        reference_get0_inline_admitted: false,
        mirror_fast_table_ptr: 0,
        mirror_fast_epoch_addr: 0,
        class_get_superclass_inline_admitted: false,
        class_get_name_inline_admitted: false,
        // Per VM: `direct_helper_table_for` asks
        // `thread_java_obj_offset_for` (nio20 w4 L3).
        thread_java_obj_offset: 0,
        vthread_state_offset: 0,
        vthread_state_mask: 0,
        // Per VM: `direct_helper_table_for` bakes `JitRealm::unsafe_inline`'s
        // cell table (nio21 wave 7 L3).
        unsafe_field_cells: 0,
        // `Integer.valueOf(I)` / `Integer.intValue()` thin direct-call helpers —
        // same no-ABI-change registration pattern as the savebase watch helpers
        // above. See `jit_integer_value_of_direct` / `jit_integer_int_value_direct`
        // and the recognition in `jit::try_compile`.
        //
        // Every helper in this block is a **VM-side reimplementation of a
        // registered native** that the JIT bakes straight into the emitted `CALL`.
        // Under `JdkOnly` that is `NativeShadowsBytecode` by construction (§1 rule
        // 4: concrete bytecode wins).
        //
        // H20-1 — WHAT THIS PARAGRAPH USED TO SAY, AND WHY IT WAS DELETED.
        //
        // Until 2026-08-21 it continued: "…so they are not registered at all. Belt
        // and braces: `jit::direct_native_helper` already refuses to *bind* a
        // non-zero address once the policy is latched, and leaving the address at
        // `0` makes that refusal unreachable rather than merely correct."
        //
        // Fifteen lines below, the registration block says "Registered
        // UNCONDITIONALLY as of 2026-08-06 (JDK-ONLY-WAVE2 §2)". Both sentences
        // have been in this function, contradicting each other, since that commit.
        // `H12-1` found and rewrote the OTHER copy of the same stale claim (the
        // thin-helper module block ~7 000 lines up); this one it did not see. Two
        // copies of one deleted premise is the shape of
        // `a-premise-in-a-comment-is-not-a-compile-time-link`: the commit that
        // removed the fact could not be made to visit either sentence that
        // depended on it.
        //
        // The refusal that IS real is `jit::direct_native_helper` at the
        // MethodEntry door, `admit_direct_native_entry` at the other two since
        // `H12-1` O1/O2 (2026-09-22), and the callee-side gate in the helper
        // bodies. This paragraph used to end "the bind-time source the other
        // two doors must switch to; O1/O2 in the H20-1 record" -- they have.
        //
        // Deliberately NOT skipped under `JdkOnly`:
        //  * `set_indy_bridge_fn` — an `invokedynamic` *bootstrap*
        //    bridge, not a native-method dispatch. The interpreter reaches the same
        //    bridge for the same sites, so gating it would move the call without
        //    changing the policy answer, while perturbing
        //    `CompiledMethod::has_indy_trap` (an OSR-correctness input).
        //  * `set_monitor_direct_fns` — VM monitor services, not registered
        //    natives. Not a dispatch site.
        // Both exclusions match `jit/src/lib.rs`'s own JDK-ONLY-NOTE items 5 and 6.
        indy_bridge: jit_indy_bridge as *const () as usize,
        // JIT round 19 proposal wave 4, lane strgap4 (STRGAP19-3): the
        // compile-time reader of a concat site's inline plan. CompilerOnly;
        // the switch (`CRATONVM_JIT_CONCAT_INLINE`) is read by the compiler.
        concat_inline_plan: jit_concat_inline_plan as *const () as usize,
        // The 32 `VarHandle` read-mode slots. Published as one array so a slot
        // can never be wired to the WRONG monomorphisation: the order here is
        // `varhandle_read_helper_slot`'s order by construction, not by a
        // hand-kept list of `set_*` calls in the right sequence.
        varhandle_read: varhandle_read_direct_fns(),
        varhandle_byte_array_view_read: varhandle_byte_array_view_read_direct_fns(),
        varhandle_byte_buffer_view_write: varhandle_byte_buffer_view_write_direct_fns(),
        system_arraycopy: jit_system_arraycopy_direct as *const () as usize,
        // JIT round 23 wave 3, lane L8 (`CRATONVM_JIT_IR_FFM_ELEMENT_DIRECT`).
        ffm_element_get: jit_ffm_element_get_direct as *const () as usize,
        ffm_element_set: jit_ffm_element_set_direct as *const () as usize,
        // JIT round 23 wave 4, lane L5 (`CRATONVM_JIT_REFLECT_NEW_ARRAY_DIRECT`).
        reflect_new_array: jit_reflect_new_array_direct as *const () as usize,
        // JIT round 23 wave 4, lane L5 (`CRATONVM_JIT_STRING_REGEX_DIRECT`).
        string_matches: jit_string_matches_direct as *const () as usize,
        string_replace: [
            jit_string_replace_direct::<false> as *const () as usize,
            jit_string_replace_direct::<true> as *const () as usize,
        ],
        // Same contract, one table over: the write slots are in
        // `varhandle_write_helper_slot`'s order by construction.
        varhandle_write: varhandle_write_direct_fns(),
        varhandle_cas: varhandle_cas_direct_fns(),
        // nio21 wave 19 (lane L3): CompilerOnly, called at compile time with
        // `static_base_resolver_ctx` (`jit_thread_local_ir_layout`).
        thread_local_ir_layout: jit_thread_local_ir_layout as *const () as usize,
        // Per VM: `direct_helper_table_for` publishes the barrier's poll page
        // pair under `CRATONVM_JIT_PAGE_POLL` (t1 wave 8, lane SAFEPOLL).
        page_poll_base: 0,
        // Per VM: `direct_helper_table_for` publishes the JVMTI event word in
        // an `experimental-debug` build (j27 wave 4, lane CLONEIR).
        jvmti_alloc_events_word: 0,
        dmha_class_id: 0,
        reflect_invoke_cache: 0,
        reflect_gc_epoch_addr: 0,
        reflect_hidden_observed_addr: 0,
        reflect_boolean_true_addr: 0,
        reflect_boolean_false_addr: 0,
        reflect_method_invoke_cache: 0,
        barrier_set: cratonvm_types::BarrierSet::default(),
    }
}

// ---------------------------------------------------------------------------
// Direct-helper signature checks (`docs/jit/helper-abi.md` §10)
// ---------------------------------------------------------------------------
//
// The twin of the `JitRuntimeHelpers` block below, for the SECOND helper table.
// `direct_helper_table()` above stores every slot as `jit_foo as *const () as
// usize`, which type-checks against nothing; these coercions are what tie each
// stored function to the `DirectHelperFn*` alias the emitter's arity
// assertions read. Changing a helper's Rust signature without changing its
// alias stops compiling here instead of miscompiling at a baked CALL.
//
// Until this block existed, `jit/src/direct_helpers.rs`'s aliases pinned the
// shape the EMITTER assumes and not the function actually stored, which is
// half a safety net: `assert_direct_helper_call_shape!` at a call site and the
// `DirectHelperFnSig` arity assertions both read the alias, so a helper whose
// Rust signature drifted away from its alias was invisible everywhere.
//
// Coverage is 36 of the 36 `DirectHelperTable` words that are ever called: the
// 31 `direct_helper_fn_slots!` rows, plus `static_base_resolver`,
// `class_initialized` and `well_known_class_id` (all stored by
// `direct_helper_table_for` rather than by `direct_helper_table`), plus the two
// served-class predicates. The one remaining word,
// `static_base_resolver_ctx`, is data — a `SharedVm` address — and is not
// callable, so there is no signature to pin.
const _: () = {
    use cratonvm_jit::direct_helpers::*;

    // Autoboxing and Object.
    let _: DirectHelperFnIntegerValueOf = jit_integer_value_of_direct;
    let _: DirectHelperFnIdentityHashCode = jit_identity_hash_code_direct;
    let _: DirectHelperFnReflectMethodInvokeSentinel = crate::jit::helpers::jit_reflect_method_invoke_sentinel;
    let _: DirectHelperFnIntegerIntValue = jit_integer_int_value_direct;
    let _: DirectHelperFnLongValueOf = jit_long_value_of_direct;
    let _: DirectHelperFnLongLongValue = jit_long_long_value_direct;

    // Collections.
    let _: DirectHelperFnHashMapPut = jit_hashmap_put_direct;
    let _: DirectHelperFnHashMapGet = jit_hashmap_get_direct;
    let _: DirectHelperFnCtorNewInstance = jit_ctor_new_instance_direct;
    let _: DirectHelperFnConcurrentHashMapGet = jit_concurrent_hashmap_get_direct;

    // Strings.
    let _: DirectHelperFnStringLatin1Lower = jit_string_latin1_to_lower_direct;

    // NIO byte accessors and `Buffer.session()`.
    let _: DirectHelperFnNioByteBufferPutByte = jit_dbb_put_byte_direct;
    let _: DirectHelperFnNioByteBufferGetByte = jit_dbb_get_byte_direct;
    let _: DirectHelperFnBufferSession = jit_buffer_session_direct;

    // `ScopedMemoryAccess`. The four PUT accessors take seven words — `val` is
    // interposed before `big_endian` — so they overflow SysV's six as well as
    // Win64's four, which is why every one of them is `JavaArgMarshalled` and
    // why `DIRECT_HELPERS_NEEDING_SYSV_STACK_ARGS == 4`. If one of these ever
    // loses or gains a word, this block is where it stops.
    let _: DirectHelperFnScopedMemoryGetInt = jit_scoped_memory_get_int_direct;
    let _: DirectHelperFnScopedMemoryGetShort = jit_scoped_memory_get_short_direct;
    let _: DirectHelperFnScopedMemoryGetLong = jit_scoped_memory_get_long_direct;
    let _: DirectHelperFnScopedMemoryGetChar = jit_scoped_memory_get_char_direct;
    let _: DirectHelperFnScopedMemoryPutInt = jit_scoped_memory_put_int_direct;
    let _: DirectHelperFnScopedMemoryPutShort = jit_scoped_memory_put_short_direct;
    let _: DirectHelperFnScopedMemoryPutLong = jit_scoped_memory_put_long_direct;
    let _: DirectHelperFnScopedMemoryPutChar = jit_scoped_memory_put_char_direct;
    let _: DirectHelperFnScopedMemoryGetByteInternal = jit_scoped_memory_get_byte_internal_direct;
    let _: DirectHelperFnScopedMemoryPutByteInternal = jit_scoped_memory_put_byte_internal_direct;
    let _: DirectHelperFnScopedMemoryGetIntInternal = jit_scoped_memory_get_int_internal_direct;
    let _: DirectHelperFnScopedMemoryGetShortInternal = jit_scoped_memory_get_short_internal_direct;
    let _: DirectHelperFnScopedMemoryGetLongInternal = jit_scoped_memory_get_long_internal_direct;
    let _: DirectHelperFnScopedMemoryGetCharInternal = jit_scoped_memory_get_char_internal_direct;
    let _: DirectHelperFnScopedMemoryPutIntInternal = jit_scoped_memory_put_int_internal_direct;
    let _: DirectHelperFnScopedMemoryPutShortInternal = jit_scoped_memory_put_short_internal_direct;
    let _: DirectHelperFnScopedMemoryPutLongInternal = jit_scoped_memory_put_long_internal_direct;
    let _: DirectHelperFnScopedMemoryPutCharInternal = jit_scoped_memory_put_char_internal_direct;
    let _: DirectHelperFnUnsafeGetByte = jit_unsafe_get_byte_direct;
    let _: DirectHelperFnUnsafeGetShort = jit_unsafe_get_short_direct;
    let _: DirectHelperFnUnsafeGetInt = jit_unsafe_get_int_direct;
    let _: DirectHelperFnUnsafeGetLong = jit_unsafe_get_long_direct;
    let _: DirectHelperFnUnsafeGetReference = jit_unsafe_get_reference_direct;
    let _: DirectHelperFnUnsafeGetReferenceVolatile = jit_unsafe_get_reference_volatile_direct;
    let _: DirectHelperFnUnsafePutByte = jit_unsafe_put_byte_direct;
    let _: DirectHelperFnUnsafePutShort = jit_unsafe_put_short_direct;
    let _: DirectHelperFnUnsafePutInt = jit_unsafe_put_int_direct;
    let _: DirectHelperFnUnsafePutLong = jit_unsafe_put_long_direct;
    let _: DirectHelperFnUnsafeCasInt = jit_unsafe_cas_int_direct;
    let _: DirectHelperFnUnsafeCasLong = jit_unsafe_cas_long_direct;
    // nio21 wave 3, lane L4.
    let _: DirectHelperFnUnsafeGetIntVolatile = jit_unsafe_get_int_volatile_direct;
    let _: DirectHelperFnUnsafeGetLongVolatile = jit_unsafe_get_long_volatile_direct;
    let _: DirectHelperFnUnsafePutIntVolatile = jit_unsafe_put_int_volatile_direct;
    let _: DirectHelperFnUnsafePutLongVolatile = jit_unsafe_put_long_volatile_direct;
    let _: DirectHelperFnUnsafePutReferenceVolatile = jit_unsafe_put_reference_volatile_direct;
    let _: DirectHelperFnUnsafeCasReference = jit_unsafe_cas_reference_direct;
    let _: DirectHelperFnUnsafeFullFence = jit_unsafe_full_fence_direct;
    let _: DirectHelperFnReferenceRefersTo0 = jit_reference_refers_to0_direct;
    let _: DirectHelperFnPhantomReferenceRefersTo0 = jit_phantom_reference_refers_to0_direct;
    let _: DirectHelperFnReferenceGet0 = jit_reference_get0_direct;
    let _: DirectHelperFnThreadCurrentCarrierThread = jit_thread_current_carrier_thread_direct;

    // MessageDigest and `Thread.currentThread()`.
    let _: DirectHelperFnMdUpdateByte = jit_md_update_byte_direct;
    let _: DirectHelperFnThreadCurrentThread = jit_thread_current_thread_direct;
    let _: DirectHelperFnSystemCurrentTimeMillis = jit_system_current_time_millis_direct;
    let _: DirectHelperFnSystemNanoTime = jit_system_nano_time_direct;
    let _: DirectHelperFnObjectNotify = jit_object_notify_direct;
    let _: DirectHelperFnObjectNotifyAll = jit_object_notify_all_direct;
    let _: DirectHelperFnUnsafeUnpark = jit_unsafe_unpark_direct;
    let _: DirectHelperFnUnsafePark = jit_unsafe_park_direct;

    // Monitors — the two `HandWritten` slots, reached through
    // `runtime_lowering::emit_monitor_stub`, which loads the argument
    // registers itself and has no stack-argument path at all.
    let _: DirectHelperFnMonitorEnter = jit_monitor_enter;
    let _: DirectHelperFnMonitorExit = jit_monitor_exit;
    // The deferred field site, hand-written by `op_field.rs`
    // `emit_deferred_field_site` (interpreter round i4 wave 92, lane L2).
    let _: DirectHelperFnFieldSiteCp = jit_field_site_cp;

    let _: DirectHelperFnPreconditionsCheckIndex = jit_preconditions_check_index_direct;
    let _: DirectHelperFnScopedMemoryCopyMemory = jit_scoped_memory_copy_memory_direct;
    let _: DirectHelperFnUnsafeCopyMemory = jit_unsafe_copy_memory_direct;
    let _: DirectHelperFnDecimalDigitsLatin1Int = jit_decimal_digits_latin1_int_direct;
    let _: DirectHelperFnDecimalDigitsLatin1Long = jit_decimal_digits_latin1_long_direct;
    let _: DirectHelperFnVectorizedHashCode = jit_vectorized_hash_code_direct;
    let _: DirectHelperFnVectorizedMismatch = jit_vectorized_mismatch_direct;
    let _: DirectHelperFnCrc32UpdateBytes0 = jit_crc32_update_bytes0_direct;
    let _: DirectHelperFnCrc32UpdateBytes = jit_crc32_update_bytes_direct;
    let _: DirectHelperFnAdler32UpdateBytes = jit_adler32_update_bytes_direct;
    let _: DirectHelperFnByteArrayClone = jit_byte_array_clone_direct;
    let _: DirectHelperFnObjectClone = jit_object_clone_direct;
    let _: DirectHelperFnObjectCloneVirtual = jit_object_clone_virtual_direct;
    let _: DirectHelperFnSbGrow = jit_sb_grow_direct;
    // `-> ()`. The only void slot outside the watchpoints and the VarHandle
    // write arm; a site that reserved a result register for it would read
    // whatever the callee happened to leave in RAX.
    let _: DirectHelperFnReachabilityFence = jit_reachability_fence_direct;
    let _: DirectHelperFnObjectGetClass = jit_object_get_class_direct;
    let _: DirectHelperFnClassGetName = jit_class_get_name_direct;
    let _: DirectHelperFnClassGetSuperclass = jit_class_get_superclass_direct;
    let _: DirectHelperFnThreadLocalGet = jit_thread_local_get_direct;
    let _: DirectHelperFnThreadLocalSet = jit_thread_local_set_direct;
    // The third parameter is a real `*const i64`, not an `i64` — a staged
    // descending word block the bridge reads `arg_count` words from, not a
    // value.
    let _: DirectHelperFnIndyBridge = jit_indy_bridge;

    // spring-bug-10 watchpoints. On Windows `jit_arm_savebase_watch` is a
    // naked trampoline; that is still a fn item and still coerces, which is
    // the point — the trampoline's hand-written asm assumes exactly this
    // `extern "C"` shape and nothing else was checking it.
    let _: DirectHelperFnArmSavebaseWatch = jit_arm_savebase_watch;
    let _: DirectHelperFnDisarmSavebaseWatch = jit_disarm_savebase_watch;

    // The three VarHandle arrays: one monomorphisation stands for the array,
    // because every element is the same generic function at a different
    // `const SLOT` and they cannot differ in signature.
    let _: DirectHelperFnVarHandleRead = jit_varhandle_read_direct::<0>;
    let _: DirectHelperFnVarHandleByteArrayViewRead =
        jit_varhandle_byte_array_view_read_direct::<0>;
    let _: DirectHelperFnSystemArraycopy = jit_system_arraycopy_direct;
    let _: DirectHelperFnFfmElementGet = jit_ffm_element_get_direct;
    let _: DirectHelperFnFfmElementSet = jit_ffm_element_set_direct;
    let _: DirectHelperFnReflectNewArray = jit_reflect_new_array_direct;
    let _: DirectHelperFnStringMatches = jit_string_matches_direct;
    let _: DirectHelperFnStringReplace = jit_string_replace_direct::<false>;
    let _: DirectHelperFnStringReplace = jit_string_replace_direct::<true>;
    let _: DirectHelperFnVarHandleByteBufferViewWrite =
        jit_varhandle_byte_buffer_view_write_direct::<0>;
    let _: DirectHelperFnVarHandleWrite = jit_varhandle_write_direct::<0>;
    let _: DirectHelperFnVarHandleCas = jit_varhandle_cas_direct::<0>;

    // The `CompilerOnly` words: every line from here to the end of this block.
    // Each is called by the COMPILER through a `transmute` in
    // `jit/src/direct_helpers.rs`, never baked into a CALL, so `extern "C"` on
    // both ends is the whole of its contract. (Some are stored by
    // `direct_helper_table_for` rather than by `direct_helper_table`, e.g.
    // `static_base_resolver`, `class_initialized`, `well_known_class_id`.) An
    // `extern "C" fn` item is called by name identically to a plain one, so
    // without these lines the `extern "C"` would be deletable without a single
    // error in the producer's crate; here the two spellings meet. A new
    // CompilerOnly word adds its line below; nothing here counts them.
    let _: DirectHelperFnStaticBaseResolver = jit_resolve_static_base;
    let _: DirectHelperFnClassInitialized = jit_class_initialized;
    let _: DirectHelperFnWellKnownClassId = jit_well_known_class_id;
    let _: DirectHelperFnConcatInlinePlan = jit_concat_inline_plan;
    // rt22: the `ThreadLocal.get()` / `Reference.refersTo` layout reader was
    // stored in `direct_helper_table` with no signature pin of its own.
    let _: DirectHelperFnThreadLocalIrLayout = jit_thread_local_ir_layout;
    let _: DirectHelperFnObjectCloneServedClass = jit_object_clone_served_class;
    let _: DirectHelperFnBufferSessionServedClass =
        cratonvm_native_builtins::buffer_session::class_is_served;
    let _: DirectHelperFnNioByteElementServedClass =
        cratonvm_native_io::direct_buffer::elem_fastpath::class_is_served;
};

// ---------------------------------------------------------------------------
// Helper-slot signature checks (`docs/jit/helper-abi.md` §1, invariant 33)
// ---------------------------------------------------------------------------
//
// `build_helpers` above stores every callable slot as
// `jit_foo as *const () as usize`. That cast type-checks against **nothing**:
// any `extern "C"` function of any arity, argument width or return type casts
// to `*const ()` just as cleanly, so the signatures declared in `jit-api`'s
// `helper_fn_slots!` — which are what the backends bake call sequences from —
// were never compared against the functions actually installed. A helper that
// grew a parameter, or whose return went from `i64` to `()`, would compile
// fine on both sides and go wrong only in generated machine code.
//
// Each line below binds one helper to its declared alias. A fn item coerces to
// a fn-pointer type only when its `extern`-ness, arity, argument types and
// return type all match, so a divergence is a compile error naming the exact
// slot. `const _` blocks emit no code and cost nothing at run time.
//
// Deliberately NOT in `build_helpers`: keeping the checks out of the function
// leaves the (already long) table literal untouched and lets the list be read
// as a census.
//
// NOTE — `get_current_thread` is absent from the list below, and cannot be
// added to it as-is. Its alias is declared `() -> *mut c_void` while
// `jit_get_current_thread` returns `*mut JvmThread`. The two are ABI-identical
// (both are thin pointers in RAX) which is why the raw cast has always
// "worked", but fn-pointer types are invariant in their return type, so the
// coercion below cannot express it and no cast can either. It is pinned
// separately, immediately after this block — see the ABI-4 pair there for what
// IS checked and what is not.
const _: () = {
    use cratonvm_jit_api::helpers_abi::*;

    // Allocation.
    let _: HelperFnNewarray = jit_newarray;
    let _: HelperFnNewObject = jit_new_object;
    let _: HelperFnAnewarrayObject = jit_anewarray_object;
    let _: HelperFnNewObjectCp = jit_new_object_cp;
    let _: HelperFnAnewarrayObjectCp = jit_anewarray_object_cp;
    let _: HelperFnLdcClassCp = jit_ldc_class_cp;
    // The compile-time link whose ABSENCE permitted the whole W7-38 /
    // W8-E11-1 family: with this line, changing `jit_aastore_type_check`'s
    // signature without the matching `helpers_abi.rs` row is a build error
    // instead of a comment that quietly stops being true.
    let _: HelperFnAastoreTypeCheck = jit_aastore_type_check;
    let _: HelperFnMultianewarray2d = jit_multianewarray_2d;
    let _: HelperFnMultianewarrayN = jit_multianewarray_n;
    let _: HelperFnTlabPostInit = jit_post_tlab_init;
    let _: HelperFnZgcNoteTlabObject = jit_zgc_note_tlab_object;

    // Monitors.
    let _: HelperFnMonitorEnter = jit_monitor_enter;
    let _: HelperFnMonitorExit = jit_monitor_exit;

    // Array access.
    let _: HelperFnBaload = jit_baload;
    let _: HelperFnBastore = jit_bastore;
    let _: HelperFnIaload = jit_iaload;
    let _: HelperFnIastore = jit_iastore;
    let _: HelperFnAaload = jit_aaload;
    let _: HelperFnAastore = jit_aastore;
    let _: HelperFnArraylength = jit_arraylength;

    // Instance fields.
    let _: HelperFnGetfield = jit_getfield;
    let _: HelperFnPutfieldInt = jit_putfield_int;
    let _: HelperFnPutfieldLong = jit_putfield_long;
    let _: HelperFnPutfieldFloat = jit_putfield_float;
    let _: HelperFnPutfieldDouble = jit_putfield_double;
    let _: HelperFnPutfieldObject = jit_putfield_object;
    let _: HelperFnG1PostWriteBarrier = jit_g1_post_write_barrier;

    // Statics.
    let _: HelperFnGetstatic = jit_getstatic;
    let _: HelperFnPutstaticInt = jit_putstatic_int;
    let _: HelperFnPutstaticLong = jit_putstatic_long;
    let _: HelperFnPutstaticFloat = jit_putstatic_float;
    let _: HelperFnPutstaticDouble = jit_putstatic_double;
    let _: HelperFnPutstaticObject = jit_putstatic_object;

    // Out-parameter helpers: the pointer argument is `*mut i64` on both sides.
    let _: HelperFnFfmSegmentGet = jit_ffm_segment_get;
    let _: HelperFnFfmSegmentSet = jit_ffm_segment_set;
    let _: HelperFnLocalHandlerLookup = jit_local_handler_lookup;

    // Type checks.
    let _: HelperFnCheckcast = jit_checkcast;
    let _: HelperFnInstanceofCheck = jit_instanceof;

    // Implicit exceptions and the out-of-band signal plumbing.
    let _: HelperFnThrowAioobe = jit_throw_aioobe;
    let _: HelperFnThrowArithmetic = jit_throw_arithmetic;
    let _: HelperFnThrowException = jit_throw_exception;
    let _: HelperFnSetThrowBci = jit_set_throw_bci;
    let _: HelperFnJitNpeWithAction = jit_npe_with_action;
    let _: HelperFnDispatchThrew = jit_dispatch_threw;

    // Invocation and deopt.
    let _: HelperFnInvokeDispatch = jit_invoke_dispatch;
    let _: HelperFnInvokeVirtualMic = jit_invoke_virtual_mic;
    let _: HelperFnServiceCalleeDeopt = jit_service_callee_deopt;
    let _: HelperFnUncommonTrap = jit_uncommon_trap;
    let _: HelperFnLambdaIntToDouble = jit_lambda_int_to_double;

    // GC barriers.
    let _: HelperFnWriteBarrier = jit_write_barrier;
    let _: HelperFnSatbPreWriteBarrier = jit_satb_pre_write_barrier;

    // Floating point.
    let _: HelperFnMathFmaDouble = jit_math_fma_double;
    let _: HelperFnMathFmaFloat = jit_math_fma_float;
    let _: HelperFnJitFrem = jit_frem;
    let _: HelperFnJitDrem = jit_drem;

    // Stack guards, precise maps, strings, safepoints.
    let _: HelperFnSelfCallStackGuard = jit_self_call_stack_guard;
    let _: HelperFnNativeStackFloor = jit_native_stack_floor;
    // Both candidates for the one `frame_record` slot — `build_helpers`
    // chooses between them at run time, so both must match the alias.
    let _: HelperFnFrameRecord = jit_frame_record;
    let _: HelperFnFrameRecord = jit_verify_inline_frame_record;
    let _: HelperFnLdcString = jit_ldc_string;
    let _: HelperFnLdcStringCp = jit_ldc_string_cp;
    let _: HelperFnSafepointSlowPath = jit_safepoint_slow_path;
};

// ---------------------------------------------------------------------------
// ABI-4 — `get_current_thread`, the one alias of 60 the block above cannot
// reach, pinned in the parts of it that CAN be checked.
// ---------------------------------------------------------------------------
//
// `HelperFnGetCurrentThread` is `unsafe extern "C" fn() -> *mut c_void`;
// `jit_get_current_thread` returns `*mut JvmThread`. Fn-pointer types are
// invariant in their return type, so neither a coercion nor an `as` cast
// relates the two — and a `transmute` between two fn-pointer types, which is
// the obvious-looking remedy, would check nothing beyond both being
// pointer-sized. It would look like a check while being exactly as empty as
// the `as *const () as usize` casts this whole file exists to replace.
//
// So pin the checkable parts separately and let them do the work together:
//
//  * the PRODUCER's full shape is spelled out below, so adding a parameter to
//    `jit_get_current_thread`, returning something that is not a
//    `*mut JvmThread`, or dropping the `extern "C"`, stops compiling there;
//  * the CONSUMER's declared arity and return-ness come from `HELPER_FN_SIGS`,
//    which `helper_fn_slots!` DERIVES from the alias rather than transcribing,
//    so editing the alias without editing the helper stops compiling on the
//    assertion.
//
// What remains unchecked, precisely: the pointee type. Nothing ties
// `JvmThread` to `c_void`, and nothing can while the alias is spelled in a
// crate that cannot name `JvmThread`.
//
// `NOTES-abi7.md` proposed finishing this by making `jit_get_current_thread`
// return `*mut c_void` and casting inside, so the plain coercion could join the
// block above and the count would read 60/60. **Declined, and the reason is not
// the cost it names.** That edit adds no check at all: it would replace the
// `-> *mut JvmThread` coercion below with a `-> *mut c_void` one, which is a
// STRICTLY WEAKER statement about the producer, in exchange for a count. A
// documented 59-of-60 with this pair beside it says more than a 60-of-60 whose
// sixtieth row asserts a type nobody wrote.
//
// What IS worth pinning about the pointee is the one property of it the ABI
// actually depends on, and it is pinned below: that `*mut JvmThread` is THIN.
// The residue has always been described as ABI-irrelevant because "both are
// thin pointers returned in RAX" — true today, and true only while `JvmThread`
// is `Sized`. Give it an unsized tail (a trait object field, a `[T]`, an
// `extern type`) and `*mut JvmThread` becomes a two-word fat pointer returned
// in RAX:RDX, while the inline-TLAB sequence in `x64` reads RAX and nothing
// else. That would be a silent, total corruption of the thread pointer on the
// allocation fast path, it is reachable by an edit to a struct in another file
// that mentions none of this, and neither the coercion below nor the
// `HELPER_FN_SIGS` assertions would notice.
const _: unsafe extern "C" fn() -> *mut JvmThread = jit_get_current_thread;
const _: () = {
    let sig = cratonvm_jit_api::helpers_abi::helper_sig_of("get_current_thread");
    assert!(
        sig.arity == 0 && sig.float_args == 0,
        "the `get_current_thread` alias declares arguments, but the helper it \
         names takes none and the inline-TLAB call site loads no argument \
         register at all — the callee would read an uninitialised register",
    );
    assert!(
        sig.returns_value,
        "the `get_current_thread` alias declares no return value, but the \
         inline-TLAB fast path reads the thread pointer out of RAX",
    );
    assert!(
        core::mem::size_of::<*mut JvmThread>() == core::mem::size_of::<*mut core::ffi::c_void>(),
        "`*mut JvmThread` is no longer a thin pointer, so this helper now \
         returns a two-word value in RAX:RDX. `HelperFnGetCurrentThread` is \
         declared `() -> *mut c_void`, and the inline-TLAB fast path reads RAX \
         alone — the thread pointer would be half a fat pointer. This is the \
         ONE ABI-relevant part of the pointee-type residue described above, \
         and it is the part the proposed `c_void` return would have hidden \
         rather than closed",
    );
};

/// GC-safe materialization for a compiled `ldc "..."` instruction.
///
/// The literal bytes live in the owning `CompiledMethod`; this helper consults
/// the VM string pool on every execution. Keeping the object reference out of
/// generated code is essential: the pool is rewritten after a moving GC, while
/// an immediate object address would become stale on the next invocation.
///
/// No emitter calls it since 2026-08-20 (both backends emit
/// [`jit_ldc_string_cp`]); it stays because its helper-table slot is a
/// `RequiredPtr`. gen r5w2/oomjit6: FALLIBLE like its twin -- `0` with a
/// pending `OutOfMemoryError` on a full heap instead of the infallible
/// constructor's `FATAL: heap exhausted` abort (item 1 of
/// `docs/feature-designs/generative-gc-architecture.md`),
/// so a caller must test the result for `0`.
#[no_mangle]
pub extern "C" fn jit_ldc_string(vm_ptr: i64, bytes: *const u8, len: usize) -> i64 {
    let f = || jit_ldc_string_body(vm_ptr, bytes, len);
    contain("jit_ldc_string", OnPanic::Throw { vm_ptr }, 0, f)
}

/// Body of [`jit_ldc_string`], run under its panic guard.
pub fn jit_ldc_string_body(vm_ptr: i64, bytes: *const u8, len: usize) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if vm_ptr == 0 || bytes.is_null() {
        return 0;
    }
    // SAFETY: the JIT compiler owns the literal bytes for the lifetime of its
    // compiled method, and `vm_ptr` is the hidden SharedVm argument installed
    // by the compiled-entry trampoline.
    let text = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(bytes, len)) };
    let shared = unsafe { &*(vm_ptr as *const SharedVm) };
    // gen r5w2/oomjit6: the ladder `jit_ldc_string_cp_body` uses -- decline,
    // one collection through the helpers' forced door, retry, then a catchable
    // `OutOfMemoryError` -- instead of the infallible pooled constructor.
    if let Some((obj, _)) = crate::vm::try_create_java_string_reporting(shared, text) {
        return obj.as_ptr() as i64;
    }
    // SAFETY: this helper runs on the JIT thread that installed the thread
    // pointer, and no other `&mut JvmThread` borrow is live here (the same
    // contract as the allocation slow paths).
    if let Some((thread, _guard)) = unsafe { jit_thread_mut() } {
        thread.tlab.retire();
        crate::runtime::interpreter::maybe_gc_forced_pub_at(shared, thread, "jit-ldc-string");
    }
    match crate::vm::try_create_java_string_reporting(shared, text).or_else(|| {
        jit_ldc_last_ditch(shared, || {
            crate::vm::try_create_java_string_reporting(shared, text)
        })
    }) {
        Some((obj, _)) => obj.as_ptr() as i64,
        None => jit_alloc_oom(shared, "Java heap space (ldc String)"),
    }
}

/// Cooperative JIT safepoint polling (`CRATONVM_JIT_SAFEPOINT_POLLS`) slow
/// path — called by JIT-compiled code when the inline poll
/// (`jit/src/x64.rs::emit_safepoint_poll`) observes the `stw_requested`
/// flag byte (`helpers.safepoint_flag_addr`, see
/// `GcBarrier::stw_requested_flag_addr`) set.
///
/// Recovers the `SharedVm` from the flag address the poll passes (falling back
/// to process-wide VM publication) and the `JvmThread` from
/// `jit_thread_mut()`'s `JIT_THREAD` TLS, then
/// joins the SAME stop-the-world wait the interpreter's own poll hit uses
/// (`crate::runtime::interpreter::safepoint_check` — retire the TLAB, drain
/// SATB, publish a fresh root snapshot, arrive at the GC barrier) so a
/// thread parked here is exactly as GC-visible as an interpreter frame at
/// its poll point.
///
/// The poll site (`emit_safepoint_poll`) always emits
/// `emit_pre_safepoint_spill()` immediately before this CALL, so every
/// register-resident local/oop is already flushed to its canonical frame
/// slot before `safepoint_check` can park this thread — the conservative
/// scanner sees a complete picture of this frame while parked.
///
/// An absent process VM or `JIT_THREAD` TLS entry is a silent no-op. Resolving
/// the VM here instead of passing the hidden context pointer lets pure
/// compiled methods use the same poll sequence as context methods.
///
/// Returns the loop-exit verdict (interpreter round i1 wave 12, lane L4):
/// non-zero when a frame must run interpreted now
/// ([`jit_safepoint_loop_exit_verdict`] has the bits), else `0`. Only
/// back-edge polls read it (`x64::emit_back_edge_safepoint_poll`, leaving
/// through its loop header's OSR-exit map; since wave 13
/// `emit_branch_back_edge_safepoint_poll`, leaving through a map at a
/// conditional back edge's own bci; since wave 15 in method-entry bodies too,
/// and the self-tail poll); every other poll ignores RAX.
///
/// `body_id` (interpreter round i1 wave 17, lane L1) is the polling body's
/// compile id, `0` when it has none. The single-pass backend passes it at
/// every poll; the IR tier passes it at every poll of a method-entry compile
/// whose back edges may leave (wave 18, lane L2:
/// `ir_lower::ir_entry_mode_exits_admitted`) and `0` elsewhere; the aarch64
/// poll loads no argument at all. The id is trusted only when it equals this thread's compile-id
/// mirror ([`polling_body_id`]), so an undefined register reads as an unnamed
/// body.
// SAFETY: called only from JIT-compiled code at a poll site emitted by
// `emit_safepoint_poll`, which always precedes the CALL with
// `emit_pre_safepoint_spill`.
#[no_mangle]
pub unsafe extern "C" fn jit_safepoint_slow_path(flag_addr: i64, body_id: i64) -> i64 {
    let f = move || jit_safepoint_slow_path_body(flag_addr, body_id);
    // `0` on a contained panic: stay compiled, the behaviour before the
    // verdict existed.
    contain(
        "jit_safepoint_slow_path",
        OnPanic::Throw { vm_ptr: 0 },
        0,
        f,
    )
}

/// Body of [`jit_safepoint_slow_path`], run under its panic guard.
// SAFETY: Called under `contain` panic guard with JIT caller arguments.
pub unsafe fn jit_safepoint_slow_path_body(flag_addr: i64, body_id: i64) -> i64 {
    pub static HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    crate::jit::conservative_roots::note_jit_boundary();
    // The flag the poll tested names its VM. `process_vm_strict` is the
    // fallback for a poll emitted without the argument (aarch64): with a
    // single VM it resolves exactly as before, and with several it refuses
    // rather than park on another VM's barrier.
    // Cast: the poll passes the flag byte's address as an i64 register value.
    let vm = match crate::native::jni::vm_for_stw_flag_addr(flag_addr as usize) {
        Some(vm) => vm,
        None => match crate::native::jni::process_vm_strict() {
            Some(vm) => vm,
            None => {
                // t1 wave 8 lane HANDSHK: no VM to drain for, but a handshake
                // arm on this OS thread's word (`CRATONVM_GC_HANDSHAKE_POLL_BIT`)
                // must still come down, or every later poll of this thread
                // comes back here until it does; the requester takes its pause
                // at its budget, as for an unserved arm.
                let _ = crate::threading::gc_barrier::take_this_threads_handshake_arm();
                return 0;
            }
        },
    };
    // Interpreter round i1 wave 27, lane L6: a forced post-call exit site's
    // stub (`cratonvm_jit::POST_CALL_EXIT_VERDICT_ONLY`) asks for the verdict
    // alone. It preserved the frame's registers instead of publishing its
    // references, so it must not park here: it parks at the body's next poll,
    // or leaves through its exit map, and a collection requested meanwhile
    // waits for that or freezes it back in compiled code, where the
    // take-over scans it conservatively (`xt_root_scan`).
    if body_id & cratonvm_jit::POST_CALL_EXIT_VERDICT_ONLY != 0 {
        let body_id = body_id & !cratonvm_jit::POST_CALL_EXIT_VERDICT_ONLY;
        let mut verdict = jit_safepoint_loop_exit_verdict(vm.as_ref(), body_id);
        // Interpreter round i2 wave 54, lane L2: a body whose post-call exits
        // leave through a caller chain stays while a chain sink would refuse
        // the chain after the call committed. Interpreter round i6 wave 118,
        // lane L2 (proposal `i47-L3-proposal-a-post-call-exit-asks-its-transfer-before-it-leaves`):
        // the question, named, is asked BEFORE the stub leaves, and its answer
        // is on the verdict line (`transfer=ready|refused`).
        let transfer = (verdict != 0).then(|| post_call_exit_transfer_ready(vm.as_ref(), body_id));
        if transfer == Some(false) {
            if dbg_deopt_on() {
                eprintln!(
                    "[cratonvm-deopt] post-call exit verdict id={body_id}: a spliced scope of \
                     its chain exits could not be rebuilt now; the frame stays (was {verdict:#x})"
                );
            }
            verdict = 0;
        }
        note_post_call_exit_verdict(vm.as_ref(), body_id, verdict, transfer);
        return verdict;
    }
    // gce e2/t: `CRATONVM_DBG_XT_FORCE_TAKEOVER` -- decline to park, back to
    // the compiled loop, so the take-over freezes this thread in compiled
    // code (bounded per pause; see `xt_root_scan::ForceTakeover`). The switch
    // is read at most once per thread, and only here, with a pause in
    // progress; off, this costs one flag load and one `Option` test.
    {
        let barrier = &vm.as_ref().mem.gc_barrier;
        if barrier
            .stw_requested
            .load(std::sync::atomic::Ordering::Acquire)
        {
            if let Some((thread, _guard)) = jit_thread_mut() {
                let generation = barrier
                    .gc_generation
                    .load(std::sync::atomic::Ordering::Acquire);
                if thread.xt_force_takeover.should_decline(generation) {
                    return 0;
                }
            }
        }
    }
    // A leftover native-return handoff must not ride into the root snapshot
    // this park publishes: the poll site spilled every live oop to its frame
    // slot first (see the doc above), so the slot is stale here by
    // construction. A PEER's collection is the one that would otherwise keep
    // it — see `drain_native_return_at_compiled_helper_entry`.
    drain_native_return_at_compiled_helper_entry();
    // JIT round 21 lane stress (FUZZ21-1 / FUZZ21-7): an arrival that found
    // the pause flag clear was forced by the back-edge poll's stress countdown
    // (`ir_lower::Lowerer::emit_poll_stress_countdown`), the only code that
    // enters here with the flag clear but for a racing release, an owed-cycle
    // latch, or a withdrawn body's forced exit poll -- each of which, under a
    // stress switch only, merely counts as one more forced arrival. With both
    // switches off this is one byte load on the slow path.
    let stress = poll_stress_arrival(vm.as_ref(), flag_addr);
    // JIT round 21 lane fuzzvm (FUZZTOOL21-1): count the forced arrival
    // (both `false` for every natural one; nothing is touched then).
    let census = &vm.as_ref().jit.bridge.poll_stress_census;
    if stress.deopt || stress.gc {
        PollStressCensus::bump(&census.arrivals);
    }
    // t1 lane E (`CRATONVM_GC_HANDSHAKE_POLL_BIT`, opt-in): a poll that read
    // this OS thread's word (`CRATONVM_JIT_POLL_THREAD_WORD`) may be here for
    // a handshake arm alone. Lower it BEFORE `safepoint_check` drains the
    // queue (`run_pending_handshakes`), so an arm that lands after the clear
    // stays up for the next poll; lowered even without a JIT thread, or every
    // later poll of this thread would come back here (its requester then
    // takes its pause at the budget). One TLS read and one relaxed load when
    // nothing is armed; never armed with the switch off.
    let _ = crate::threading::gc_barrier::take_this_threads_handshake_arm();
    if let Some((thread, _guard)) = jit_thread_mut() {
        let hit = HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if hit <= 16 && false {
            eprintln!(
                "[jit-safepoint] cooperative slow-path hit={} thread_id={}",
                hit, thread.thread_id.0
            );
        }
        crate::runtime::interpreter::safepoint_check(vm.as_ref(), thread);
        // gcg g4/oo hunk 5.2 (a), applied by g4/jm (opt-in
        // `CRATONVM_GEN_CONC_OWED_POLL`): an owed Generational concurrent cycle
        // runs here, outside any allocation; the poll site has spilled the
        // frame's references, so the cycle's pauses scan it as a parked frame.
        // Below the verdict-only early return (that stub published nothing)
        // and the forced take-over's decline. One relaxed load when nothing
        // is latched.
        crate::runtime::interpreter::gc_and_alloc::gen_owed_concurrent_cycle_at_poll(
            vm.as_ref(),
            thread,
        );
        // FUZZ21-7 (`CRATONVM_DBG_GC_AT_BACK_EDGE`): a collection at this
        // poll. The poll published its frame's map before calling here, so
        // this thread's compiled frames are rooted exactly as a parked
        // thread's are at a pause another thread requested.
        if stress.gc {
            poll_stress_collect(vm.as_ref(), thread);
            PollStressCensus::bump(&census.collections);
        }
    } else if stress.gc {
        // FUZZTOOL21-1: a forced arrival with no attached thread collects nothing.
        PollStressCensus::bump(&census.gc_no_thread);
    }
    // ── Interpreter round i1 wave 12, lane L4: the loop-exit verdict. ──
    // Asked LAST, after the park (and anything else above it), so a mode an
    // agent switched on while this thread was parked is seen now.
    let mut verdict = jit_safepoint_loop_exit_verdict(vm.as_ref(), body_id);
    // FUZZ21-1 (`CRATONVM_DBG_JIT_DEOPT_A_LOT`): a forced arrival nothing
    // else sends out leaves anyway, when its body may (before the chain check
    // below, which still applies to it).
    let mut forced_exit = false;
    if stress.deopt {
        if verdict == 0 {
            verdict = jit_safepoint_stress_exit_verdict(vm.as_ref(), body_id);
            forced_exit = verdict != 0;
        } else {
            // FUZZTOOL21-1: another mode's verdict already sends the body out.
            PollStressCensus::bump(&census.preempted);
        }
    }
    // JIT round 23 wave 1, lane L1 (THROWS22-1, `CRATONVM_JIT_OSR_PROFIT_EXIT`):
    // a single-pass OSR body armed because an optimizing OSR body for its
    // method's headers is ready leaves at this poll. Asked only when nothing
    // else told the frame to leave, and only of a poll (not of a post-call
    // exit site's verdict-only call above). One load while nothing is armed.
    let mut profit_body: Option<usize> = None;
    let verdict = if verdict == 0 && vm.as_ref().jit.jit_cache.osr_profit_exits_armed() != 0 {
        profit_body = osr_profit_exit_body(vm.as_ref(), body_id);
        if profit_body.is_some() {
            cratonvm_jit::SAFEPOINT_VERDICT_INNERMOST_FRAME
        } else {
            0
        }
    } else {
        verdict
    };
    // Interpreter round i2 wave 57, lane L2: a body whose back-edge polls
    // leave through a caller chain (a loop inside a splice) stays while a
    // chain sink would refuse the chain, as its post-call exits do. Only on
    // a "leave" verdict: nothing on the common path.
    if verdict != 0 && !chain_poll_exits_resumable_for(vm.as_ref(), body_id) {
        if dbg_deopt_on() {
            eprintln!(
                "[cratonvm-deopt] chain poll exit verdict id={body_id}: a spliced scope of its \
                 chain exits could not be rebuilt now; the frame stays (was {verdict:#x})"
            );
        }
        if forced_exit {
            PollStressCensus::bump(&census.refused_chain);
        }
        // JIT round 23 wave 1, lane L1 (review item 1): a profit exit this
        // frame cannot take would leave it paying a slow-path call at every
        // back edge; take the arm back.
        if let Some(ptr) = profit_body {
            // SAFETY: as in `osr_profit_exit_body`: the frame running the body
            // owns it until this verdict is answered.
            let cm = unsafe { &*(ptr as *const cratonvm_jit::CompiledMethod) };
            disarm_osr_profit_exit_noted(vm.as_ref(), cm, "a chain sink refused");
        }
        if forced_exit {
            PollStressCensus::bump(&census.refused_chain);
        }
        return 0;
    }
    if forced_exit {
        PollStressCensus::bump(&census.granted);
    }
    // Review item 2: a profit verdict is counted only once nothing can
    // cancel it.
    if let Some(ptr) = profit_body {
        // SAFETY: as above.
        let cm = unsafe { &*(ptr as *const cratonvm_jit::CompiledMethod) };
        let n = vm.as_ref().jit.jit_cache.note_osr_profit_exit_verdict();
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr profit exit verdict #{n}: {} id={} -> leave for the \
                 optimizing OSR body",
                cm.method_label, cm.compile_id
            );
        }
    }
    verdict
}

/// What a safepoint slow-path arrival must do for the round-21 stress
/// switches (FUZZ21-1 / FUZZ21-7). Both `false` for every natural arrival and
/// for every arrival with both switches off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PollStressArrival {
    /// `CRATONVM_DBG_JIT_DEOPT_A_LOT`: send the polling body out.
    deopt: bool,
    /// `CRATONVM_DBG_GC_AT_BACK_EDGE`: collect here.
    gc: bool,
}

/// JIT round 21 lane stress: classify one slow-path arrival. A forced arrival
/// is one whose pause flag -- the VM's `stw_requested` byte, which the poll
/// named by passing its address as `flag_addr` -- is clear: the stress
/// countdown jumped past the test. Read through the barrier, never through the
/// raw argument (a poll that passes no flag address hands over garbage), and
/// first, so a natural arrival -- the flag set -- never reads a switch.
pub fn poll_stress_arrival(vm: &SharedVm, flag_addr: i64) -> PollStressArrival {
    let barrier = &vm.mem.gc_barrier;
    // Cast: the poll hands the flag byte's address over as an i64 register.
    if flag_addr == 0
        || barrier.stw_requested_flag_addr() as usize != flag_addr as usize
        || barrier
            .stw_requested
            .load(std::sync::atomic::Ordering::Acquire)
    {
        return PollStressArrival::default();
    }
    poll_stress_arrival_from(
        cratonvm_jit::ir_lower::dbg_poll_stress_switch_on("CRATONVM_DBG_JIT_DEOPT_A_LOT"),
        cratonvm_jit::ir_lower::dbg_poll_stress_switch_on("CRATONVM_DBG_GC_AT_BACK_EDGE"),
    )
}

/// The switch half of [`poll_stress_arrival`], for the unit tests.
pub fn poll_stress_arrival_from(deopt: bool, gc: bool) -> PollStressArrival {
    PollStressArrival { deopt, gc }
}

/// JIT round 21 lane fuzzvm (FUZZTOOL21-1): this VM's account of the forced
/// back-edge poll arrivals of `CRATONVM_DBG_JIT_DEOPT_A_LOT` /
/// `CRATONVM_DBG_GC_AT_BACK_EDGE` (`JitBridgeState::poll_stress_census`, one
/// per VM, no static). The per-event `FUZZ21-1 deopt-a-lot: ... told to
/// leave` line names only the exits a forced arrival was granted; this also
/// counts the arrivals that were REFUSED, by reason, so a stress run that
/// never sent a probe's loop out says whether the mode never reached the loop
/// (`arrivals=0`) or reached it and was refused every time. Bumped only on a
/// forced arrival (the slow path, under a stress switch); printed once per VM
/// by [`report_poll_stress_census_at_exit`] from both `vm-cli` shutdown arms.
/// Debug-only: nothing here changes what an arrival does.
#[derive(Default)]
pub struct PollStressCensus {
    /// Forced arrivals (the pause flag clear, a stress switch set).
    pub arrivals: std::sync::atomic::AtomicU64,
    /// Under deopt-a-lot: forced exits granted (the sinks' verdict, past the
    /// chain check).
    pub granted: std::sync::atomic::AtomicU64,
    /// Refused: the poll named no compiled body this thread runs.
    pub refused_unnamed: std::sync::atomic::AtomicU64,
    /// Refused: the body was withdrawn by a class redefinition.
    pub refused_withdrawn: std::sync::atomic::AtomicU64,
    /// Refused: the body is no `PollingBody` (an optimizing OSR body has no
    /// owner class / label to resume through).
    pub refused_not_polling_body: std::sync::atomic::AtomicU64,
    /// Refused: `interpreter::stress_exit_verdict` answered 0 (the body's
    /// exits cannot be resumed now, or the VM saw a redefinition).
    pub refused_sinks: std::sync::atomic::AtomicU64,
    /// Refused after a granted verdict: a spliced scope of the body's chain
    /// exits could not be rebuilt (`chain_poll_exits_resumable_for`).
    pub refused_chain: std::sync::atomic::AtomicU64,
    /// Under deopt-a-lot: another mode's verdict already sent the body out,
    /// so no forced exit was asked.
    pub preempted: std::sync::atomic::AtomicU64,
    /// Collections taken at a forced arrival (gc-at-back-edge).
    pub collections: std::sync::atomic::AtomicU64,
    /// gc-at-back-edge arrivals with no attached `JvmThread` (no collection).
    pub gc_no_thread: std::sync::atomic::AtomicU64,
    /// The exit line was printed (both shutdown arms may reach the report).
    pub printed: std::sync::atomic::AtomicBool,
}

impl PollStressCensus {
    #[inline]
    pub fn bump(cell: &std::sync::atomic::AtomicU64) {
        cell.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// The exit line. `refused` is the sum of the five reasons; `arrivals`
    /// also counts gc-only and preempted arrivals, so it is not
    /// `granted + refused`.
    pub fn census_line(&self) -> String {
        use std::sync::atomic::Ordering::Relaxed;
        let un = self.refused_unnamed.load(Relaxed);
        let wd = self.refused_withdrawn.load(Relaxed);
        let np = self.refused_not_polling_body.load(Relaxed);
        let sk = self.refused_sinks.load(Relaxed);
        let ch = self.refused_chain.load(Relaxed);
        format!(
            "[cratonvm-deopt] FUZZ21-1 forced arrivals={} granted={} refused={} \
             refused-unnamed={un} refused-withdrawn={wd} refused-not-polling-body={np} \
             refused-sinks={sk} refused-chain={ch} preempted={} collections={} \
             gc-no-thread={}",
            self.arrivals.load(Relaxed),
            self.granted.load(Relaxed),
            un + wd + np + sk + ch,
            self.preempted.load(Relaxed),
            self.collections.load(Relaxed),
            self.gc_no_thread.load(Relaxed),
        )
    }
}

/// JIT round 21 lane fuzzvm (FUZZTOOL21-1): print this VM's
/// [`PollStressCensus`] line once, when either poll-stress switch is set
/// (`CRATONVM_DBG=jit-deopt-a-lot=..` / `gc-at-back-edge=..`), so a zero is a
/// reading, not silence; nothing otherwise. Called from both `vm-cli`
/// shutdown arms (the `System.exit` pre-exit hook and the normal-return
/// teardown); the per-VM `printed` latch makes a run that reaches both print
/// once.
pub fn report_poll_stress_census_at_exit(shared: &SharedVm) {
    let deopt = cratonvm_jit::ir_lower::dbg_poll_stress_switch_on("CRATONVM_DBG_JIT_DEOPT_A_LOT");
    let gc = cratonvm_jit::ir_lower::dbg_poll_stress_switch_on("CRATONVM_DBG_GC_AT_BACK_EDGE");
    if !deopt && !gc {
        return;
    }
    let census = &shared.jit.bridge.poll_stress_census;
    if census
        .printed
        .swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        return;
    }
    eprintln!("{} [at exit]", census.census_line());
}

/// FUZZ21-7: request and take part in a collection at a compiled back-edge
/// poll, through the allocation-threshold door's `gc_requested` latch
/// (`maybe_gc`, the latch `jcmd GC.run` sets): the collection, reference
/// processing and finalizer / cleaner drains every other door runs. Another
/// thread may win the latch first; this thread then takes part in that
/// pause instead, which still collects at this poll.
#[cold]
#[inline(never)]
pub fn poll_stress_collect(vm: &SharedVm, thread: &mut crate::threading::jvm_thread::JvmThread) {
    vm.mem
        .gc_requested
        .store(true, std::sync::atomic::Ordering::Relaxed);
    crate::runtime::interpreter::maybe_gc(vm, thread);
}

/// FUZZ21-1: the verdict a forced arrival gets under
/// `CRATONVM_DBG_JIT_DEOPT_A_LOT` when nothing else asked the body to leave:
/// `SAFEPOINT_VERDICT_POLLING_BODY` for a NAMED method-entry body whose exits
/// the sinks can resume (`interpreter::stress_exit_verdict`), `0` otherwise
/// (an unnamed body, an optimizing OSR body -- no owner class, so no
/// `PollingBody` -- a withdrawn one, or a VM where a class was redefined). The
/// poll then leaves through its mode exit at the loop header, the precise
/// state that exit already describes, and the sinks resume it uncharged.
///
/// # Safety
/// As [`jit_safepoint_loop_exit_verdict`].
#[cold]
#[inline(never)]
pub unsafe fn jit_safepoint_stress_exit_verdict(vm: &SharedVm, body_id: i64) -> i64 {
    let mirror = crate::jit::conservative_roots::top_cm_id_mirror_read();
    let artifact = polling_body_id(body_id, mirror)
        .and_then(|id| cratonvm_jit::lookup_compile_id(id).or_else(|| running_osr_body_named(id)));
    // SAFETY: as in `jit_safepoint_loop_exit_verdict`: the id is the one this
    // thread's innermost compiled frame published, and that frame (or the OSR
    // door, for an unpublished body) owns the artifact until it returns.
    // FUZZTOOL21-1: each refusal below is counted by reason.
    let census = &vm.jit.bridge.poll_stress_census;
    let Some(cm) = artifact.map(|ptr| unsafe { &*(ptr as *const cratonvm_jit::CompiledMethod) })
    else {
        PollStressCensus::bump(&census.refused_unnamed);
        return 0;
    };
    if vm.jit.jit_cache.body_withdrawn_by_redefinition(cm) {
        PollStressCensus::bump(&census.refused_withdrawn);
        return 0;
    }
    let Some(body) = crate::runtime::interpreter::PollingBody::from_artifact(
        cm.owner_class_id,
        &cm.method_label,
    ) else {
        PollStressCensus::bump(&census.refused_not_polling_body);
        return 0;
    };
    let points = cm.deopt_points.as_ptr_range();
    // Cast: point addresses, compared only (`owns_deopt_point`).
    let body = body
        .with_artifact_facts(
            (points.start as usize, points.end as usize),
            cm.install_epoch,
        )
        .with_boxed_exit_points(&cm._deopt_point_boxes);
    let verdict = crate::runtime::interpreter::stress_exit_verdict(vm, body);
    if verdict == 0 {
        PollStressCensus::bump(&census.refused_sinks);
    }
    if verdict != 0 && dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] FUZZ21-1 deopt-a-lot: {} told to leave at a forced poll",
            cm.method_label
        );
    }
    verdict
}

/// The OSR profit exit's question for the body that polled (JIT round 23 wave
/// 1, lane L1; THROWS22-1): the body's artifact (as a pointer) when it is a
/// single-pass OSR body the VM armed for a profit exit
/// (`JitCache::osr_profit_exit_may_leave`), so the slow path answers
/// [`cratonvm_jit::SAFEPOINT_VERDICT_INNERMOST_FRAME`]; `None` otherwise.
/// Counts nothing: the slow path counts the verdict once the chain check can
/// no longer cancel it (item 2 of
/// `docs/feature-designs/deopt-osr.md`).
///
/// The body is named the way the OSR door's continuation record names it
/// first ([`running_osr_body_named`]: the artifact the door runs for this
/// thread's innermost interpreter frame, when its compile id is the one the
/// poll passed), so the verdict is about the frame whose activation the body
/// runs in -- the frame the innermost-frame bit tells to leave, and the only
/// one an OSR body's poll acts for. With that record off it falls back to the
/// compile-id mirror ([`polling_body_id`]), as the redefinition verdict does.
/// Only an OSR body tests that bit (`x64::safepoint::emit_safepoint_poll_leaving_to`),
/// so a method-entry body polling under an armed OSR frame keeps running.
///
/// Review item 1: an ARMED body neither route can name would be told nothing
/// at every back edge while its forced polls keep calling here. On x86-64 the
/// poll's id argument is its own compile id, baked as an immediate, so the
/// raw id still finds it: its arm is taken back
/// ([`disarm_osr_profit_exit_noted`]) and its polls restored.
#[cold]
#[inline(never)]
pub fn osr_profit_exit_body(vm: &SharedVm, body_id: i64) -> Option<usize> {
    let id = u32::try_from(body_id).ok().filter(|&id| id != 0)?;
    let mirror = crate::jit::conservative_roots::top_cm_id_mirror_read();
    let named = running_osr_body_named(id)
        .or_else(|| polling_body_id(body_id, mirror).and_then(cratonvm_jit::lookup_compile_id));
    let Some(ptr) = named else {
        if cfg!(target_arch = "x86_64") {
            if let Some(raw) = cratonvm_jit::lookup_compile_id(id) {
                // SAFETY: on x86-64 every poll passes its own compile id
                // (`emit_safepoint_body_id_arg`), so `raw` is the body that is
                // polling on this thread right now, alive until it returns.
                let cm = unsafe { &*(raw as *const cratonvm_jit::CompiledMethod) };
                if cm.compile_id == id && cm.osr_profit_exit_armed() {
                    disarm_osr_profit_exit_noted(vm, cm, "the poll could not be named");
                }
            }
        }
        return None;
    };
    // SAFETY: as in `jit_safepoint_loop_exit_verdict`: the frame running the
    // body (or the OSR door, which holds its `Arc` while it runs it) owns the
    // artifact until it returns, which it cannot do before this verdict is
    // answered.
    let cm = unsafe { &*(ptr as *const cratonvm_jit::CompiledMethod) };
    (cm.compile_id == id && vm.jit.jit_cache.osr_profit_exit_may_leave(cm)).then_some(ptr)
}

/// Take back the profit-exit arm of `cm`, whose frame could not leave
/// (`why`), and say so under `CRATONVM_DBG_JITC` (review item 1 of
/// `docs/feature-designs/deopt-osr.md`).
#[cold]
#[inline(never)]
pub fn disarm_osr_profit_exit_noted(vm: &SharedVm, cm: &cratonvm_jit::CompiledMethod, why: &str) {
    let restored = vm.jit.jit_cache.disarm_osr_profit_exit(cm);
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] osr profit exit DISARMED {} id={}: {why}; polls restored={restored} \
             (disarms={})",
            cm.method_label,
            cm.compile_id,
            vm.jit.jit_cache.osr_profit_exit_disarms()
        );
    }
}

/// [`post_call_chain_exits_resumable`] for the body a back-edge poll named,
/// asked only of a body whose polls leave through a caller chain
/// (`CompiledMethod::chain_poll_exits`: a loop inside a spliced callee of an
/// optimizing compile, `CRATONVM_JIT_IR_SPLICED_HEADER_POLL_EXITS` /
/// `CRATONVM_JIT_IR_ENTRY_SPLICED_HEADER_POLL_EXITS`). `true` for every other
/// body, so a flat poll exit's verdict is unchanged. Interpreter round i2
/// wave 57, lane L2.
#[cold]
#[inline(never)]
pub fn chain_poll_exits_resumable_for(vm: &SharedVm, body_id: i64) -> bool {
    let mirror = crate::jit::conservative_roots::top_cm_id_mirror_read();
    let artifact = polling_body_id(body_id, mirror)
        .and_then(|id| cratonvm_jit::lookup_compile_id(id).or_else(|| running_osr_body_named(id)));
    // SAFETY: as in `jit_safepoint_loop_exit_verdict`: the id is the one this
    // thread's innermost compiled frame published, and that frame (or the OSR
    // door, for an unpublished body) owns the artifact until it returns, which
    // it cannot do before this verdict is answered.
    let Some(cm) = artifact.map(|ptr| unsafe { &*(ptr as *const cratonvm_jit::CompiledMethod) })
    else {
        return true;
    };
    !cm.chain_poll_exits || post_call_chain_exits_resumable(vm, cm)
}

/// May the polling METHOD-ENTRY body `cm`, forced to leave as its class's
/// obsolete activation after a renumbering redefinition
/// (`cratonvm_jit::not_entrant::OWN_CLASS_RENUMBERED_ENTRY_BODIES_LEAVE`), be
/// told to leave (interpreter round i1 wave 46, lane L2)? Only when a frame
/// rebuilt in the bytecode it was compiled from, stamped with that
/// bytecode's pool generation, translates onto the class's current pool
/// (`obsolete_frames::rebuilt_body_translates`, the question
/// `deopt_resume::obsolete_activation_source` asks at a trap); otherwise its
/// exit would be refused by every sink and re-run from entry, so it stays
/// compiled, as before. Asked once per force (the answer is memoised on the
/// body, `CompiledMethod::renumbered_entry_resumes`): one class-manager read
/// on the slow path of a forced body, never on a poll's fast path.
pub fn renumbered_entry_body_resumes(vm: &SharedVm, cm: &cratonvm_jit::CompiledMethod) -> bool {
    if let Some(known) = cm.renumbered_entry_resumes() {
        return known;
    }
    // The sinks would refuse it with either kill switch off: stay.
    if !crate::runtime::interpreter::obsolete_entry_resume_switches_on() {
        return false;
    }
    // Never blocks: a forced post-call exit asks this verdict with its frame's
    // references unpublished, and must not wait on a lock a redefinition (or
    // a collection's requester) may hold. Busy: stay this time, unmemoised,
    // and ask again at the body's next exit.
    let Some(classes) = vm.classes.class_manager.try_read() else {
        return false;
    };
    let resumes = cm
        .compiled_source()
        .zip(cm.compile_cp_stamp())
        .is_some_and(|(source, stamp)| {
            crate::runtime::interpreter::obsolete_frames::rebuilt_body_translates_in(
                &classes,
                source.declaring_class_id,
                &source.code,
                &source.exception_table,
                stamp,
            )
        });
    drop(classes);
    cm.note_renumbered_entry_resumes(resumes);
    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] renumbered method-entry body {}: its frame translates={resumes} \
             ({})",
            cm.method_label,
            if resumes {
                "told to leave; a stash sink rebuilds it in its own bytecode"
            } else {
                "stays compiled, as before wave 46"
            },
        );
    }
    resumes
}

/// May a forced post-call exit of `cm` that leaves through a CALLER CHAIN (a
/// real call made inside a splice: interpreter round i1 wave 47's OSR-door
/// sites, round i2 wave 54's method-entry ones) be told to leave now?
/// Interpreter round i2 wave 54, lane L2.
///
/// Such an exit is taken AFTER the call committed, so the sink that rebuilds
/// the chain must not refuse it: a refusal re-runs the method from entry (the
/// method-entry doors) or abandons the transfer (the OSR door), and either
/// repeats or skips what the activation already did. Every chain sink
/// rebuilds a spliced scope whose class was redefined since the compile from
/// the bytecode the compile spliced (`CompiledMethod::splice_scope_sources`,
/// `deopt_resume::chain_inner_scope_own_sources`), and refuses when that
/// bytecode is missing or its frame would not translate onto the class's
/// current pool. This asks the same question of every spliced scope any of
/// the body's chain exit points names, before the exit is taken: `false` (the
/// verdict becomes "stay", and the frame runs on to its next exit-capable
/// back edge, as before wave 47) when
///
/// * such a scope has no class id recorded, or its class was redefined since
///   the compile and has no kept bytecode, or a kept body that does not
///   translate, or the own-source switches are off
///   (`CRATONVM_DEOPT_CHAIN_INNER_OWN_SOURCE`,
///   `CRATONVM_DEOPT_CHAIN_OWN_SOURCE_SINKS`);
/// * a METHOD-ENTRY body's own class was redefined since its compile (its
///   chain's outermost frame would need the body's own template, a case this
///   check does not vouch for);
/// * the class manager is busy (`try_read`: never blocks; asked again at the
///   next exit).
///
/// `true` at once for a body with no chain exit point, and while no class was
/// ever redefined. Asked only when the post-call verdict would send the frame
/// out (cold: a site calls the verdict only after a redefinition forced it).
///
/// Will the transfer a forced post-call exit of `body_id` leaves into succeed,
/// asked BEFORE the stub leaves (interpreter round i6 wave 118, lane L2;
/// `i47-L3-proposal-a-post-call-exit-asks-its-transfer-before-it-leaves`)?
/// A post-call exit leaves after its call committed, so a transfer refused
/// after it would repeat or skip what the activation did; "no" makes the
/// verdict "stay" and the frame runs on to its next exit-capable back edge.
///
/// The transfer's only refusal that depends on what happened AFTER the
/// compile is a chain sink's: a spliced scope whose class was redefined and
/// has no kept bytecode that still translates
/// ([`post_call_chain_exits_resumable`]). A flat point's refusals (fit,
/// virtual objects, the verify checks) are compile-time facts
/// (`osr_exit::guard_exit_point_resumable`), so for a body without chain
/// points this is `true`. Per BODY, not per point: a body with one
/// unrebuildable chain point stays at every forced site. Conservative (a
/// stay, never a re-run); the per-point form needs the stub to pass its
/// point (the proposal's item 1), not built.
#[cold]
#[inline(never)]
pub fn post_call_exit_transfer_ready(vm: &SharedVm, body_id: i64) -> bool {
    let artifact = u32::try_from(body_id)
        .ok()
        .and_then(|id| cratonvm_jit::lookup_compile_id(id).or_else(|| running_osr_body_named(id)));
    // SAFETY: as in `note_post_call_exit_verdict`: the id is the body whose
    // stub called the slow path on this thread, and the frame running it (or
    // the OSR door, for an unpublished body) owns the artifact until it
    // returns, which it cannot do before this verdict is answered.
    let Some(cm) = artifact.map(|ptr| unsafe { &*(ptr as *const cratonvm_jit::CompiledMethod) })
    else {
        // An unnamed body has no chain this could check: its verdict stands.
        return true;
    };
    post_call_chain_exits_resumable(vm, cm)
}

/// Was the running activation of `cm` -- a body a compiled caller bound for a
/// caller-held synchronized `CALL` (`CompiledMethod::is_bound_by_a_caller_held_site`)
/// -- entered by a VM door rather than by that caller? Interpreter round i2
/// wave 54, lane L2: the per-activation answer of
/// `docs/feature-designs/real-jdk-mode.md`
/// ("Fixed (wave 54)": a positive door test instead of return-address
/// tables), behind `CRATONVM_JIT_SYNC_LEAF_DOOR_ACTIVATION_EXITS`.
///
/// Asked only of a body whose polls leave on `SAFEPOINT_VERDICT_NAMED_BODY`
/// alone (`CompiledMethod::named_poll_exits_only`): a call-free body, so the
/// activation that polls is the innermost compiled frame. The door that
/// entered it pushed the thread's innermost JIT entry, naming this very
/// artifact, from a frame above the activation
/// (`conservative_roots::innermost_entry_names`, with the RBP mirror the
/// verdict's named body published); a compiled caller's direct `CALL` pushes
/// no entry, so a caller-held activation never passes. A door holds the
/// method monitor in its guard and hands it to the resumed frame
/// (`hand_monitor_to_resumed_frame`), exactly as for a body no caller bound,
/// so the named bit may then be set for this activation. `false` (keep the
/// per-artifact mark, wave 53) for anything else. The switch is **default
/// ON** since the wave-54 lane's last commit (`=0` restores wave 53's
/// per-artifact answer).
pub fn bound_body_activation_entered_by_a_door(cm: &cratonvm_jit::CompiledMethod) -> bool {
    cm.named_poll_exits_only
        && crate::jit::conservative_roots::innermost_entry_names(
            cm,
            crate::jit::conservative_roots::top_rbp_mirror_read(),
        )
}

/// [`post_call_exit_transfer_ready`] for the artifact `cm` itself.
pub fn post_call_chain_exits_resumable(vm: &SharedVm, cm: &cratonvm_jit::CompiledMethod) -> bool {
    if !crate::classloading::any_class_redefined() {
        return true;
    }
    // The spliced scopes (every scope but the outermost) of every chain exit
    // point: the only scopes a chain sink rebuilds for these exits.
    let mut chain_keys: Vec<&str> = Vec::new();
    for point in cm._deopt_point_boxes.iter() {
        if point.reason != cratonvm_jit::deopt::DeoptReason::OsrExit {
            continue;
        }
        let mut scope = Some(&point.frame_state);
        while let Some(frame) = scope {
            if frame.caller.is_some() && !chain_keys.contains(&frame.method_key.as_str()) {
                chain_keys.push(frame.method_key.as_str());
            }
            scope = frame.caller.as_deref();
        }
    }
    if chain_keys.is_empty() {
        return true;
    }
    // A chain scope the compile recorded no class id for cannot be judged:
    // stay (fail closed).
    if chain_keys.iter().any(|key| {
        !cm.splice_scope_class_ids
            .iter()
            .any(|(k, _)| k.as_str() == *key)
    }) {
        return false;
    }
    // The JIT cache's barrier question first, with no class-manager guard
    // held (the order `interpreter_only_obsolete_source` keeps).
    let cache = &vm.jit.jit_cache;
    let not_compiled_since = |class_id: crate::classloading::ClassId| {
        !cache.compiled_since_redefinition_of(class_id, cm.install_epoch)
    };
    let own_class = (!cm.is_osr_body()
        && cm.owner_class_id != cratonvm_types::jit_activation::NO_OWNER_CLASS)
        .then(|| crate::classloading::ClassId::new(cm.owner_class_id))
        .filter(|&class_id| not_compiled_since(class_id));
    let spliced: Vec<(&str, crate::classloading::ClassId)> = cm
        .splice_scope_class_ids
        .iter()
        .filter(|(key, _)| chain_keys.contains(&key.as_str()))
        .map(|(key, id)| (key.as_str(), crate::classloading::ClassId::new(*id)))
        .filter(|&(_, class_id)| not_compiled_since(class_id))
        .collect();
    let Some(classes) = vm.classes.class_manager.try_read() else {
        return false;
    };
    let classes: &crate::classloading::ClassManager = &classes;
    if own_class.is_some_and(|class_id| classes.class_redefine_generation(class_id) > 0) {
        return false;
    }
    let own_sources_on = cratonvm_jit::chain_inner_own_source_enabled();
    for (key, class_id) in spliced {
        if classes.class_redefine_generation(class_id) == 0 {
            continue;
        }
        if !own_sources_on {
            return false;
        }
        let Some(stamp) = cm.compile_cp_stamp() else {
            return false;
        };
        let Some(row) = cm
            .splice_scope_sources
            .iter()
            .find(|row| row.method_key == key)
        else {
            return false;
        };
        if !crate::runtime::interpreter::obsolete_frames::rebuilt_body_translates_in(
            classes,
            class_id,
            &row.code,
            &[],
            stamp,
        ) {
            return false;
        }
    }
    true
}

/// The bytecode, and its pool stamp, a stash sink must resume the exit of the
/// polling METHOD-ENTRY body `cm` in, when an agent or a debugger request
/// needs the interpreter and `cm` is its class's obsolete activation -- its
/// class was redefined since it was compiled (interpreter round i1 wave 47,
/// lane L2; `cratonvm_jit::not_entrant::INTERPRETER_ONLY_OBSOLETE_BODIES_LEAVE`,
/// `docs/feature-designs/real-jdk-mode.md`).
///
/// `Some` only when the switch is on, no wave-46 renumbering mark already
/// carries the source, an interpreter-only request is armed, the body is
/// not an OSR body (the OSR door transfers such an exit into its own live
/// frame, on the innermost-frame bit), a sink could rebuild its frame
/// (`CompiledMethod::entry_resumable_as_obsolete`, and the resume kill
/// switches on), its bytecode's class was redefined (generation above zero)
/// and not compiled from since (`JitCache::compiled_since_redefinition_of`),
/// and a frame rebuilt in that bytecode, stamped with its pool generation,
/// translates onto the class's current pool
/// (`obsolete_frames::rebuilt_body_translates_in`). The grant then carries the
/// source, `jvmti_events::polling_body_must_leave` lets the body leave, and
/// the sinks resume it there (`jit_bridge::granted_obsolete_activation_source`,
/// which asks the translation again).
///
/// Never blocks (`try_read`: busy means no source this time, unmemoised).
/// The class-manager answer is memoised on the body per process
/// redefinition count (`CompiledMethod::obsolete_entry_resumes_at`), so a
/// forced body that must stay pays it once per redefinition, not per back
/// edge. Asked only on the verdict's slow path.
pub(crate) fn interpreter_only_obsolete_source<'a>(
    vm: &SharedVm,
    cm: &'a cratonvm_jit::CompiledMethod,
) -> Option<crate::runtime::interpreter::ObsoleteEntrySource<'a>> {
    if !cratonvm_jit::INTERPRETER_ONLY_OBSOLETE_BODIES_LEAVE
        || !crate::classloading::any_class_redefined()
        || cm.leaves_as_renumbered_obsolete()
        // An OSR-door compile only: its polls leave on the innermost-frame
        // bit, into the frame the door transfers to. An optimizing METHOD-ENTRY
        // compile that merely carries OSR entry stubs (`is_osr_body`) polls on
        // the polling-body bit like any method-entry body, so a door-entered
        // activation of it needs this source too; asking `is_osr_body` here
        // left such an obsolete activation compiled under every
        // interpreter-only request (the i41 page, shape 1 (b);
        // `I41ObsoleteLockingLoops`, row `plain`).
        || cm.compiled_via_osr
        || !crate::runtime::interpreter::compiled_frames_may_be_asked_to_leave(vm)
    {
        return None;
    }
    let Some((source, stamp)) = cm.compiled_source().zip(cm.compile_cp_stamp()) else {
        // The other half of the i41 page's shape 1 (b): a body whose publish
        // site stamped no compiled source cannot be rebuilt in its own
        // bytecode, so it stays compiled. Named, so a run can count them.
        if dbg_deopt_on() {
            eprintln!(
                "[cratonvm-deopt] obsolete method-entry body {} under an interpreter-only request has no compiled source (source={} stamp={}): stays compiled",
                cm.method_label,
                cm.compiled_source().is_some(),
                cm.compile_cp_stamp().is_some()
            );
        }
        return None;
    };
    let class_id = source.declaring_class_id;
    // The memo first: a forced body that must stay reads one atomic per poll.
    let count = crate::classloading::class_redefinition_count();
    let resumes = match cm.obsolete_entry_resumes_at(count) {
        Some(known) => known,
        None => {
            // A body compiled since its class's last redefinition runs the
            // current bytecode: `polling_body_must_leave` has no refusal for
            // it, and it needs no source (not memoised: it leaves now).
            if vm
                .jit
                .jit_cache
                .compiled_since_redefinition_of(class_id, cm.install_epoch)
                || !crate::runtime::interpreter::obsolete_entry_resume_switches_on()
            {
                return None;
            }
            // The bytecode scan of `entry_resumable_as_obsolete` runs once
            // per redefinition count, under the memo.
            if !cm.entry_resumable_as_obsolete_for_the_interpreter() {
                cm.note_obsolete_entry_resumes_at(count, false);
                return None;
            }
            let classes = vm.classes.class_manager.try_read()?;
            let resumes = classes.class_redefine_generation(class_id) > 0
                && crate::runtime::interpreter::obsolete_frames::rebuilt_body_translates_in(
                    &classes,
                    class_id,
                    &source.code,
                    &source.exception_table,
                    stamp,
                );
            drop(classes);
            cm.note_obsolete_entry_resumes_at(count, resumes);
            if dbg_deopt_on() {
                eprintln!(
                    "[cratonvm-deopt] obsolete method-entry body {} under an interpreter-only \
                     request: its frame translates={resumes} ({})",
                    cm.method_label,
                    if resumes {
                        "told to leave; a stash sink rebuilds it in its own bytecode"
                    } else {
                        "stays compiled, as before wave 47"
                    },
                );
            }
            resumes
        }
    };
    // Shape 1 (b) of the i41 page
    // (`cratonvm_jit::INTERPRETER_ONLY_OBSOLETE_LOCKING_BODIES_LEAVE`): an
    // `ACC_SYNCHRONIZED` body leaves only from an activation whose resuming
    // sink hands the method monitor over -- a self-locking body (its exits
    // name the hold, and every sink resumes it owning it) or an activation a
    // VM door entered (the door holds the monitor in its guard) -- or one a
    // caller-held site made, once that site asks the callee-deopt service
    // before it releases (shape 2 (c),
    // `cratonvm_jit::caller_held_callee_resume_enabled`): every other entry
    // into such a body is that site's direct `CALL`. Asked per poll, on this
    // thread: not memoised.
    if resumes
        && source.is_synchronized
        && !(cm.self_locks_monitor
            || cratonvm_jit::caller_held_callee_resume_enabled()
            || crate::jit::conservative_roots::innermost_entry_names(
                cm,
                crate::jit::conservative_roots::top_rbp_mirror_read(),
            ))
    {
        return None;
    }
    resumes.then_some(crate::runtime::interpreter::ObsoleteEntrySource { source, stamp })
}

/// The verdict [`jit_safepoint_slow_path`] returns: the
/// `cratonvm_jit::SAFEPOINT_VERDICT_*` bits of
/// `interpreter::compiled_frame_exit_verdict`, `0` when nothing must leave.
///
/// `SAFEPOINT_VERDICT_INNERMOST_FRAME` is about the thread's innermost
/// interpreter frame (`interpreter::compiled_loop_must_leave`: a JVMTI
/// interpreter-only event, or a JDWP request concerning its method). That is
/// the right frame exactly for an OSR body, which runs inside that frame's
/// activation; an OSR body's poll tests that bit alone (the whole register
/// until wave 17). Since interpreter round i1 wave 15 a method-entry body's
/// poll leaves too (an optimizing one since wave 18), but tests
/// `SAFEPOINT_VERDICT_POLLING_BODY` alone. Since wave 17 that bit is about
/// the polling body's own method when the poll names the body
/// ([`polling_body_id`]); otherwise it is the answer about every method
/// (`interpreter::every_compiled_frame_may_leave`).
///
/// Costs nothing on the poll's fast path; on the slow path, a few per-VM
/// loads when no agent listens (the body is looked up only when one does).
///
/// # Safety
/// As [`jit_safepoint_slow_path`]: called on a mutator thread from its slow
/// path, after any earlier `jit_thread_mut` borrow has ended.
pub unsafe fn jit_safepoint_loop_exit_verdict(vm: &SharedVm, body_id: i64) -> i64 {
    // Interpreter round i1 wave 23, lane L6: a body a class redefinition
    // withdrew may be told to leave too (`PollingBody::withdrawn_by_redefinition`),
    // with no agent listening. One load while no redefinition withdrew any.
    if !crate::runtime::interpreter::compiled_frames_may_be_asked_to_leave(vm)
        && vm.jit.jit_cache.bodies_withdrawn_by_redefinition() == 0
    {
        return 0;
    }
    let mirror = crate::jit::conservative_roots::top_cm_id_mirror_read();
    // Wave 24 (lane L6): an optimizing OSR body is never published, so its
    // compile id is reserved but never bound (`bind_compile_id` runs in
    // `JitCache::put` / `put_osr` only) and `lookup_compile_id` answers `None`
    // for it: wave 23's withdrawn OSR body was never named here, and its frame
    // kept running the old splice (`RedefineRunningSpliceProbe`). The OSR
    // door's continuation record names it instead.
    let artifact = polling_body_id(body_id, mirror)
        .and_then(|id| cratonvm_jit::lookup_compile_id(id).or_else(|| running_osr_body_named(id)));
    // SAFETY: the id is the one this thread's innermost compiled frame
    // published, and that frame is the one executing this call: its artifact
    // is owned by the frame until it returns (the root walk's
    // `published_innermost_method` relies on the same fact; for an OSR body
    // the door holds its `Arc` for as long as it runs).
    let artifact = artifact.map(|ptr| unsafe { &*(ptr as *const cratonvm_jit::CompiledMethod) });
    // Marked, or compiled before a whole-cache redefinition (wave 24: a body
    // no scan reached, such as an OSR body the memo had already forgotten).
    let withdrawn = artifact.is_some_and(|cm| vm.jit.jit_cache.body_withdrawn_by_redefinition(cm));
    // Wave 23 (lane L6): an optimizing OSR body is never published, so it has
    // no owner class and `from_artifact` would not name it. When a class
    // redefinition withdrew it, the frame it runs in -- the thread's innermost
    // interpreter frame, whose method its label must name -- supplies the
    // class.
    let osr_frame_owner = artifact
        .filter(|cm| {
            cm.owner_class_id == cratonvm_types::jit_activation::NO_OWNER_CLASS && withdrawn
        })
        .and_then(|cm| {
            // SAFETY: as for the borrow below: no earlier borrow is live, and
            // this one ends with the closure.
            let (thread, _guard) = unsafe { jit_thread_mut() }?;
            let frame = thread.frames.last()?;
            let (rest, descriptor) = cm.method_label.rsplit_once(':')?;
            let (class_name, method_name) = rest.rsplit_once('.')?;
            (frame.class_name() == class_name
                && frame.method_name() == method_name
                && frame.method_descriptor() == descriptor)
                .then(|| frame.class_id.as_u32())
        });
    let body = artifact.and_then(|cm| {
        // Wave 18: with the facts the exit sinks judge the stash by (its point
        // range) and the body's age against the last redefinition.
        let points = cm.deopt_points.as_ptr_range();
        let body = crate::runtime::interpreter::PollingBody::from_artifact(
            osr_frame_owner.unwrap_or(cm.owner_class_id),
            &cm.method_label,
        );
        body.map(|body| {
            // Cast: point addresses, compared only (`owns_deopt_point`).
            body.with_artifact_facts(
                (points.start as usize, points.end as usize),
                cm.install_epoch,
            )
            // Wave 19 (lane L2): an optimizing body's poll exits are
            // `_deopt_point_boxes`, outside that range; the grant keeps their
            // addresses too, so its sinks match its stash (wave 18 gave such a
            // body no facts and the wave-17 rule). Empty for a single-pass body.
            .with_boxed_exit_points(&cm._deopt_point_boxes)
            // Wave 23 (lane L6): withdrawn by a class redefinition.
            .with_withdrawn_by_redefinition(withdrawn)
            // Interpreter round i2 wave 53 (lane L2): bound for a caller-held
            // synchronized CALL, which withholds `SAFEPOINT_VERDICT_NAMED_BODY`.
            // Interpreter round i2 wave 54 (lane L2): asked per ACTIVATION
            // for a named-only body a door entered
            // (`bound_body_activation_entered_by_a_door`).
            // Shape 2 (c) of the i41 page: not withheld at all once every
            // caller-held site asks the callee-deopt service before it
            // releases (`cratonvm_jit::caller_held_callee_resume_enabled`).
            .with_caller_held_binding(
                cm.is_bound_by_a_caller_held_site()
                    && !bound_body_activation_entered_by_a_door(cm)
                    && !cratonvm_jit::caller_held_callee_resume_enabled(),
            )
            // Wave 45 (lane L2): an OSR body of its own class's old bytecode
            // that a renumbering redefinition forced to leave. Wave 46 (lane
            // L2): or a method-entry one, once its frame is known to translate;
            // the grant then carries the bytecode its sinks resume it in.
            .with_leaves_as_renumbered_obsolete(
                cm.leaves_as_renumbered_obsolete()
                    && (!cm.leaves_as_renumbered_obsolete_entry()
                        || renumbered_entry_body_resumes(vm, cm)),
            )
            // Wave 47 (lane L2): the source only for a marked body that may
            // leave (a memo read): `polling_body_must_leave` now lets a body
            // with a source past its redefinition clause, and a marked body
            // whose frame does not translate must not be.
            .with_obsolete_source(
                (cm.leaves_as_renumbered_obsolete_entry() && renumbered_entry_body_resumes(vm, cm))
                    .then(|| cm.compiled_source().zip(cm.compile_cp_stamp()))
                    .flatten()
                    .map(
                        |(source, stamp)| crate::runtime::interpreter::ObsoleteEntrySource {
                            source,
                            stamp,
                        },
                    )
                    // Wave 47 (lane L2): or any obsolete method-entry body an
                    // interpreter-only request needs to leave, once its frame
                    // is known to translate.
                    .or_else(|| interpreter_only_obsolete_source(vm, cm)),
            )
        })
    });
    let verdict = match jit_thread_mut() {
        Some((thread, _guard)) => {
            crate::runtime::interpreter::compiled_frame_exit_verdict(vm, Some(&*thread), body)
        }
        None => crate::runtime::interpreter::compiled_frame_exit_verdict(vm, None, body),
    };
    // Wave 24 (lane L6): the positive control for a withdrawn body's exit, one
    // line per verdict that sends one out (its exit then shows as the sink's
    // own `OSR-exit TRANSFER` / resume line).
    if withdrawn && verdict != 0 && dbg_deopt_on() {
        if let Some(cm) = artifact {
            eprintln!(
                "[cratonvm-deopt] withdrawn body told to leave: {} verdict={verdict:#x}",
                cm.method_label
            );
        }
    }
    verdict
}

/// Count, and under `CRATONVM_DBG_DEOPT` report, one verdict a forced
/// post-call exit site asked for (interpreter round i1 wave 28, lane L6): the
/// positive control the wave-27 post-call exits lacked. `body_id` is the id
/// the site's stub passed, without the verdict-only bit.
///
/// The line names the body, whether this thread's compile-id mirror confirmed
/// the id (`named=false` means the verdict could not be about this body, so
/// it stayed: the cause to look at first when a forced site never leaves),
/// whether the body is withdrawn, and the verdict. Every verdict that sends
/// the frame out is printed; of the others ("stay"), the first 64 of the VM.
/// Cold: a site calls here only after a redefinition forced it.
#[cold]
#[inline(never)]
pub fn note_post_call_exit_verdict(vm: &SharedVm, body_id: i64, verdict: i64, transfer: Option<bool>) {
    let n = vm.jit.jit_cache.note_post_call_exit_verdict(verdict != 0);
    if !dbg_deopt_on() || (verdict == 0 && n > 64) {
        return;
    }
    let mirror = crate::jit::conservative_roots::top_cm_id_mirror_read();
    let named = polling_body_id(body_id, mirror);
    let artifact = u32::try_from(body_id)
        .ok()
        .and_then(|id| cratonvm_jit::lookup_compile_id(id).or_else(|| running_osr_body_named(id)));
    // SAFETY: as in `jit_safepoint_loop_exit_verdict`: the id is the body
    // whose stub called here on this thread, and the frame running it (or the
    // OSR door, for an unpublished body) owns the artifact until it returns.
    let artifact = artifact.map(|ptr| unsafe { &*(ptr as *const cratonvm_jit::CompiledMethod) });
    let (label, withdrawn) = match artifact {
        Some(cm) => (
            cm.method_label.as_str(),
            vm.jit.jit_cache.body_withdrawn_by_redefinition(cm),
        ),
        None => ("<unnamed body>", false),
    };
    eprintln!(
        "[cratonvm-deopt] post-call exit verdict #{n}: {label} id={body_id} named={} \
         withdrawn={withdrawn} verdict={verdict:#x} transfer={}",
        named.is_some(),
        match transfer {
            Some(true) => "ready",
            Some(false) => "refused",
            None => "not-asked",
        }
    );
}

/// The optimizing OSR body running this thread's innermost interpreter frame,
/// when its compile id is `id` (interpreter round i1 wave 24, lane L6), as
/// `Arc::as_ptr(..) as usize`. Such a body is entered only through the VM's
/// OSR memo and never published, so its reserved compile id is never bound
/// and `cratonvm_jit::lookup_compile_id` cannot name it; the OSR door records
/// the artifact it runs for exactly as long as it runs
/// (`jit_bridge::live_osr_continuation_artifact`). `None` when the frame is
/// not an OSR continuation, when the record is off
/// (`CRATONVM_JIT_NO_OSR_PC_REFRESH`), or when that artifact is not the body
/// that polled (a compiled callee of it has another id).
pub fn running_osr_body_named(id: u32) -> Option<usize> {
    let frame_count = {
        // SAFETY: called from the safepoint slow path's verdict on the
        // thread that installed `JIT_THREAD`, while no other borrow of it is
        // live; this one ends with the block.
        let (thread, _guard) = unsafe { jit_thread_mut() }?;
        thread.frames.len()
    };
    osr_continuation_body_named(frame_count, id)
}

/// [`running_osr_body_named`] for a thread with `frame_count` interpreter
/// frames: the artifact the OSR door recorded for the innermost one, when its
/// compile id is `id`. The half a test can drive without a JIT thread.
pub fn osr_continuation_body_named(frame_count: usize, id: u32) -> Option<usize> {
    if id == 0 {
        return None;
    }
    let cm_ptr = crate::runtime::interpreter::jit_bridge::live_osr_continuation_artifact(
        frame_count.checked_sub(1)?,
    )?;
    if cm_ptr == 0 {
        return None;
    }
    // SAFETY: the OSR door publishes a continuation only while it holds the
    // artifact's `Arc` and runs it on this thread, so the payload is alive
    // for the length of this read.
    let cm = unsafe { &*(cm_ptr as *const cratonvm_jit::CompiledMethod) };
    (cm.compile_id == id).then_some(cm_ptr)
}

/// The compile id a safepoint poll named in its second argument, when it is
/// one this thread's compile-id mirror confirms: a non-zero `u32` equal to
/// `mirror`, the identity the innermost compiled frame's prologue published
/// (and every call it made re-published on return). Anything else — `0`, a
/// register a backend left undefined, a stale mirror — is `None`, an unnamed
/// body. Interpreter round i1 wave 17, lane L1.
pub fn polling_body_id(body_id: i64, mirror: u32) -> Option<u32> {
    let id = u32::try_from(body_id).ok()?;
    (id != 0 && id == mirror).then_some(id)
}

/// Stage 3 (precise oop maps) — record the EXACT RBP of the JIT frame that is
/// about to run, called once from the JIT prologue (gated on
/// `CRATONVM_PRECISE_JIT_MAPS`). The Rust-side `JitEntryGuard` pushed a chain
/// entry just before transferring control to compiled code, but it could only
/// capture an approximate stack pointer; this fills in the precise frame base
/// so the GC root walker can address oop-map slots as `[rbp - offset]`.
///
/// `extern "C"` with the single `rbp` argument in the platform's first
/// integer-argument register, matching the JIT's `ARG_REGS[0]` load.
pub extern "C" fn jit_frame_record(rbp: usize) {
    crate::jit::conservative_roots::set_top_frame_base(rbp);
}

/// Step 1 (`docs/feature-designs/precise-jit-maps-default.md`) debug self-check
/// helper (`CRATONVM_DBG_VERIFY_INLINE_FRAME_RECORD`).
///
/// When inline frame-record AND the verify knob are both on, the JIT prologue
/// calls this immediately AFTER its inline `mov gs:[disp], rbp` (it is wired
/// into the `frame_record` helper slot for that combination, see
/// `build_helpers`). It reads the innermost-RBP mirror back through the SAME
/// accessor the GC root walk uses and asserts it equals the RBP the inline
/// store should have written — i.e. that the baked `gs:[disp]` slot is exactly
/// the slot the Rust side reads. Logs on mismatch (never panics); pure
/// validation aid with no effect on the mirror value.
pub extern "C" fn jit_verify_inline_frame_record(rbp: usize) {
    let got = crate::jit::conservative_roots::top_rbp_mirror_read();
    if got != rbp {
        // RT-5 — `writeln!` on a locked handle, NOT `eprintln!`. This helper
        // is on `helper_guard.rs`'s "deliberately NOT guarded — must stay
        // panic-free" list, and `eprintln!` panics when stderr is closed. An
        // unguarded panic here unwinds out of an `extern "C"` function, which
        // since Rust 1.81 aborts the process on the spot — so a debug
        // self-check would have become a JVM crash on a run whose stderr had
        // been closed. `helper_guard::helper_panicked` avoids the macro for
        // exactly this reason and says so.
        //
        // Reachable only under `CRATONVM_DBG_VERIFY_INLINE_FRAME_RECORD`, so
        // this is hygiene rather than a live defect — but the reason it is on
        // the unguarded list is that it is supposed to be trivially
        // panic-free, and a macro that can panic makes that claim false by
        // inspection.
        use std::io::Write as _;
        let _ = writeln!(
            std::io::stderr(),
            "[VERIFY-INLINE-FR] mismatch: inline store rbp={rbp:#x} but mirror reads {got:#x}"
        );
    }
}

/// T1.1.28 — Math.fma(double, double, double) runtime helper.
///
/// Called from JIT code via an absolute CALL emitted by the
/// `MATH_FMA_DOUBLE_INTRINSIC` arm in `jit/src/x64/op_invoke.rs` when it
/// does not emit `VFMADD213SD` inline (no FMA3, or
/// `CRATONVM_JIT_FMA_INLINE=0`). JLS `Math.fma`: `a*b + c` as if with
/// unlimited intermediate precision, rounded once.
///
/// Round 12 wave 8 (lane rt3): NOT `f64::mul_add`. This build enables no
/// `+fma` target feature, so `mul_add` was a call into the C runtime's
/// `fma`, correctly rounded only if that CRT is. The shared form is the FMA3
/// instruction when the CPU has it and an exact software fma otherwise; the
/// `StrictMath.fma` natives use the same function, so the tiers agree.
/// `CRATONVM_MATH_FMA_EXACT=0` restores `mul_add`.
#[no_mangle]
pub extern "C" fn jit_math_fma_double(a: f64, b: f64, c: f64) -> f64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    cratonvm_native_builtins::phases_late::java_fma_f64(a, b, c)
}

/// T1.1.28 — Math.fma(float, float, float) runtime helper. The `float`
/// twin of [`jit_math_fma_double`], through the same shared function (whose
/// software form rounds to odd in `f64` before the single rounding to `f32`,
/// where a CRT `fmaf` may round twice).
#[no_mangle]
pub extern "C" fn jit_math_fma_float(a: f32, b: f32, c: f32) -> f32 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    cratonvm_native_builtins::phases_late::java_fma_f32(a, b, c)
}

/// IR FP tier (Slice A) — `frem` runtime helper.
///
/// Called from JIT code via an absolute `CALL` emitted by the IR `Op::Rem`
/// Float arm (`jit/src/ir_lower.rs`), which loads the two operands into
/// XMM0/XMM1 (the float ABI's first two argument registers on both Win64 and
/// SysV) and reads the result back from XMM0.
///
/// JVMS `frem` is the truncated remainder `a - (a / b rounded toward zero) * b`
/// taking the sign of the dividend — exactly C `fmod` and Rust's `f32 %`. The
/// special cases also match the JVMS table: `frem(x, ±∞) = x`, `frem(±∞, y) =
/// NaN`, `frem(x, ±0) = NaN`, `frem(±0, y) = ±0`, and any NaN operand yields
/// NaN. There is no single SSE instruction for it, hence the helper.
#[no_mangle]
pub extern "C" fn jit_frem(a: f32, b: f32) -> f32 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary), mirroring the FMA helpers.
    crate::jit::conservative_roots::note_jit_boundary();
    // HotSpot's default NaN for an invalid remainder (wave 98, lane L7).
    crate::runtime::interpreter::java_frem(a, b)
}

/// IR FP tier (Slice A) — `drem` runtime helper. The double analogue of
/// [`jit_frem`]; the IR `Op::Rem` Double arm `CALL`s it with the operands in
/// XMM0/XMM1 and reads the remainder from XMM0. Rust's `f64 %` is `fmod`,
/// matching the JVMS `drem` semantics exactly.
#[no_mangle]
pub extern "C" fn jit_drem(a: f64, b: f64) -> f64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // `fmod`, except an invalid remainder is HotSpot's default NaN
    // `0xFFF8…`, not the Windows CRT's `0x7FF8…` (wave 98, lane L7).
    crate::runtime::interpreter::java_drem(a, b)
}








/// Call a freshly emitted thunk once and check it against the constant its
/// impl must return.
///
/// This is the part of the probe that does NOT need the failure to reproduce.
/// Every other arm can only screen calls that happen to occur, and a thunk's
/// calls are invisible to Rust entirely — but a thunk for a constant-returning
/// impl is a pure function with a known answer, so it can simply be RUN once,
/// here, at the moment it is built. A mis-emitted slide, a wrong entry or a
/// stale cached thunk shows up on the first run of the process rather than in
/// one run out of ten.
///
/// Safe to call with whatever arguments are to hand precisely because the impl
/// is a constant body: it reads none of them. The receiver is the real proxy,
/// so a capture load still addresses a live object.
///
/// Returns `false` if the thunk gave the wrong answer, in which case the caller
/// must not install it.
// SAFETY: `entry` is a freshly JIT-compiled thunk with matching ABI.
pub(crate) unsafe fn const_thunk_self_test(
    site: &crate::runtime::interpreter::LambdaJitSite,
    entry: usize,
    needs_ctx: bool,
    vm_ptr: i64,
    receiver_ref: ObjectRef,
) -> bool {
    let Some(expected) = site.const_return() else {
        return true;
    };
    // `(receiver, samArgs…)` — the shape the emitted cascade would pass.
    let mut args = [0i64; 8];
    args[0] = receiver_ref.as_ptr() as i64; // Cast: JIT ABI -- pointer to i64
    let sam_args = site.total_args() - site.num_captures();
    let incoming = 1 + sam_args;
    if incoming > args.len() {
        return true;
    }
    let Some(rc) = try_call_compiled_entry_reentrant(entry, needs_ctx, vm_ptr, &args[..incoming])
    else {
        return true;
    };
    crate::runtime::interpreter::const_probe_screen(
        expected,
        rc,
        site.impl_class_name(),
        site.impl_method_name(),
        "thunk-self-test",
    );
    (rc as i32) == expected // Cast: the Java value is the low half
}

// ===========================================================================
// FFM element-accessor fast path
// ===========================================================================
//
// `MemorySegment.getAtIndex`/`setAtIndex` are the FFM ELEMENT accessors, driven
// one element at a time by any segment-backed array. Through the ordinary
// native dispatch funnel they measure ~1158 ns/element, against ~0.8 ns for a
// `short[]` element and ~303 ns for `Unsafe.getShort(long)` — a maximally lean
// native through the SAME funnel. So ~300 ns is the funnel and ~850 ns is the
// segment native's ~10 `NativeContext` round-trips for scope liveness, address
// and size.
//
// These two helpers replace BOTH costs on the hot shape: a direct call from
// compiled code (no dispatch, no `Vec<Value>` marshalling, no registry lookup)
// into a body that does a handful of loads.
//
// # What makes this safe
//
// The helpers do NOT re-implement the segment liveness model. That model spans
// two synthetic classes owned by two different files, and this repository has
// already been bitten once by a second copy of one of its slot indices (the
// W7-89 note on `PE_ARENA_CLASS`: the duplicate made the check silently DEAD).
// Instead:
//
//   * the NATIVE publishes a verdict (`ffm_fast::note_validated`) at the point
//     where it has already proven the carrier is a plain 6-slot native segment
//     with a live scope, and, for writes, that the write was permitted;
//   * these helpers only ask `ffm_fast::is_validated`, and decline otherwise.
//
// A decline returns 0 and the caller falls through to the unchanged native
// dispatch, so every case this does not positively recognise keeps today's
// behaviour — including every exception, which the native raises exactly as
// before.
//
// The verdict is keyed by `(carrier address, epoch)`; freeing native memory,
// closing a shared session and every GC cycle bump the epoch (a confined close
// drops its owner's verdicts, the only ones it can have), which is what lets an
// address stand in for object identity in between. Each access runs inside an
// `ffm_fast::AccessWindow`, which a shared-arena close waits for before it
// frees. See `cratonvm_native_builtins::ffm_fast`.
//
// The carrier's `ptr`/`size`/`offset` are RE-READ here on every call rather
// than cached, so a slot rewrite cannot leave a stale address behind.

/// Layout kind codes shared with the JIT's intrinsic resolver. Derived at
/// compile time from the call site's DESCRIPTOR, which names the `ValueLayout`
/// subtype (`getAtIndex:(Ljava/lang/foreign/ValueLayout$OfShort;J)S`), so the
/// element width is a compile-time constant and never a runtime layout probe.
pub const FFM_KIND_BYTE: i64 = 0;
/// See [`FFM_KIND_BYTE`].
pub const FFM_KIND_SHORT: i64 = 1;
/// See [`FFM_KIND_BYTE`].
pub const FFM_KIND_CHAR: i64 = 2;
/// See [`FFM_KIND_BYTE`].
pub const FFM_KIND_INT: i64 = 3;
/// See [`FFM_KIND_BYTE`].
pub const FFM_KIND_LONG: i64 = 4;
/// See [`FFM_KIND_BYTE`].
pub const FFM_KIND_FLOAT: i64 = 5;
/// See [`FFM_KIND_BYTE`].
pub const FFM_KIND_DOUBLE: i64 = 6;

/// The low bits of the helpers' third argument that carry the kind code.
///
/// Round 12 wave 5 (lane ffm2): the third argument is `layout | kind`. The
/// call site ORs the kind (0..=6) into the LAYOUT reference it pops, which a
/// heap object's 8-byte alignment leaves free, so the helper can decline for a
/// layout the native has not read in host byte order (a `withOrder(BIG_ENDIAN)`
/// one) and check the layout's alignment against the absolute address, with no
/// change to the four-register helper ABI. Before this the layout was popped
/// and dropped, and both checks were impossible
/// (`r12w4-hunter2-ffm-element-access-ignores-layout-order-and-alignment`).
pub const FFM_KIND_MASK: i64 = 7;

/// Split the helpers' `layout | kind` argument into `(layout address, kind)`.
#[inline]
pub fn ffm_split_layout_kind(layout_kind: i64) -> (u64, i64) {
    // Cast: an object address, compared as a key and never dereferenced here.
    (
        (layout_kind & !FFM_KIND_MASK) as u64,
        layout_kind & FFM_KIND_MASK,
    )
}

/// The alignment mask the native published for `layout` (see
/// `ffm_fast::layout_align_mask`), or `None` to decline.
#[inline]
pub fn ffm_layout_align_mask(layout: u64) -> Option<u64> {
    cratonvm_native_builtins::ffm_fast::layout_align_mask(layout)
}

/// Byte width of a kind code, or `None` for one this fast path does not decode.
#[inline]
pub fn ffm_kind_width(kind: i64) -> Option<i64> {
    Some(match kind {
        FFM_KIND_BYTE => 1,
        FFM_KIND_SHORT | FFM_KIND_CHAR => 2,
        FFM_KIND_INT | FFM_KIND_FLOAT => 4,
        FFM_KIND_LONG | FFM_KIND_DOUBLE => 8,
        _ => return None,
    })
}

/// The CratonVM-minted native carrier's slots, as `native-builtins` writes
/// them. Pinned against that crate's own constants by
/// `ffm_carrier_slots_match_native_builtins`.
pub const FFM_SLOT_PTR: usize = 0;
/// See [`FFM_SLOT_PTR`].
pub const FFM_SLOT_SIZE: usize = 1;
/// See [`FFM_SLOT_PTR`].
pub const FFM_SLOT_OFFSET: usize = 5;
/// The only carrier shape this fast path decodes — matched by the publish gate
/// in `native-builtins`' `ffm_publish_verdict`.
pub const FFM_CARRIER_SLOTS: u32 = 6;

/// Read one LEGACY 16-byte `Value` cell of `obj_ptr` as an `i64`, or `None` if
/// the slot does not hold a `Long`.
///
/// Deliberately legacy-only: a COMPACT instance is declined by the caller, so
/// this never has to guess a packed offset for a class that declares no fields.
///
/// # Safety
/// `obj_ptr` must be a live object with more than `index` slots.
#[inline]
// SAFETY: the caller has established `obj_ptr` is a live carrier and that
// `index` is below its `num_slots`, so the 16-byte cell at
// `HEADER_SIZE + index * SLOT_SIZE` lies inside the allocation.
pub unsafe fn ffm_read_long_slot(obj_ptr: i64, index: usize) -> Option<i64> {
    let ptr = (obj_ptr as *const u8).add(HEADER_SIZE + index * SLOT_SIZE);
    match jit_read_value_cell_checked(ptr as *const Value, "ffm_read_long_slot") {
        Value::Long(v) => Some(v),
        _ => None,
    }
}

/// Shared prologue: decline unless `seg` is a carrier this fast path may
/// decode, returning `(base_address, byte_size)` on success.
///
/// # Safety
/// Called from compiled code with `seg` an object reference or 0.
#[inline]
// SAFETY: every dereference below is guarded first. `plausible_heap_pointer`
// rejects null and any non-heap word; the header reads that follow sit at fixed
// offsets inside the header it just established; and slot reads happen only
// after `num_slots` confirms the 6-slot carrier shape.
pub unsafe fn ffm_resolve_carrier(seg: i64, want_write: bool) -> Option<(u64, i64)> {
    if !cratonvm_types::plausible_heap_pointer(seg as u64) {
        return None;
    }
    // The verdict. Everything the native checked — carrier shape, scope
    // liveness, and for a write that the write was permitted — is condensed
    // here, and none of it is re-derived.
    if !cratonvm_native_builtins::ffm_fast::is_validated(seg as u64, want_write) {
        return None;
    }
    // Defence in depth, one load each. The verdict was published for a 6-slot
    // LEGACY carrier; re-assert both rather than trust that nothing re-shaped
    // the object, because the alternative to a mismatch is decoding slots that
    // mean something else. The compact flag first: only a LEGACY instance has
    // a shape word at `NUM_SLOTS_OFFSET` (a compact one keeps its first field
    // there), so the shape is read only once the flag says it exists.
    let gc_flags = std::ptr::read((seg as *const u8).add(cratonvm_types::GC_FLAGS_BYTE_OFFSET));
    if gc_flags & cratonvm_types::GC_FLAG_COMPACT != 0 {
        return None;
    }
    let num_slots =
        std::ptr::read((seg as *const u8).add(cratonvm_types::NUM_SLOTS_OFFSET) as *const u32);
    if num_slots != FFM_CARRIER_SLOTS {
        return None;
    }
    // RE-READ, never cached: these are ordinary mutable slots.
    let ptr = ffm_read_long_slot(seg, FFM_SLOT_PTR)?;
    let size = ffm_read_long_slot(seg, FFM_SLOT_SIZE)?;
    let offset = ffm_read_long_slot(seg, FFM_SLOT_OFFSET)?;
    if size <= 0 {
        return None;
    }
    // `segment_address` is `[0] + [5]`; mirror it exactly.
    let base = (ptr as u64).checked_add(offset as u64)?;
    if base == 0 {
        return None;
    }
    Some((base, size))
}

/// Bounds-check `index` for `width` bytes against `size`, returning the byte
/// offset. Mirrors the native's `0 <= offset && offset + width <= size` with the
/// same overflow guard; anything it would refuse is declined here instead, so
/// the native raises the exception.
#[inline]
pub fn ffm_checked_offset(index: i64, width: i64, size: i64) -> Option<i64> {
    if index < 0 {
        return None;
    }
    let offset = index.checked_mul(width)?;
    let end = offset.checked_add(width)?;
    if end > size {
        return None;
    }
    Some(offset)
}

/// SBR4 lane segacc: one entry of a thread's real-segment access memo -- the
/// [`cratonvm_native_builtins::lang_invoke::SegRealFacts`] of one `(VM, segment,
/// layout, ffm epoch, site carrier)`.
#[derive(Clone, Copy)]
pub struct RealSegEntry {
    vm: usize,
    seg: i64,
    layout: i64,
    epoch: u64,
    site_kind: u8,
    hit: RealSegHit,
}

/// What a hit hands the serving road: the facts, and where the session's
/// `state` word lives (a compact instance stores it tagless at a byte
/// displacement, a legacy one in a 16-byte `Value` cell).
#[derive(Clone, Copy)]
pub struct RealSegHit {
    facts: cratonvm_native_builtins::lang_invoke::SegRealFacts,
    state_off: usize,
    state_compact: bool,
}

pub const REAL_SEG_WAYS: usize = 8;

thread_local! {
    pub static REAL_SEG_ENTRIES: std::cell::RefCell<([Option<RealSegEntry>; REAL_SEG_WAYS], usize)> =
        const { std::cell::RefCell::new(([None; REAL_SEG_WAYS], 0)) };
}

/// The memoised facts for this access, if any (no context, no field read).
#[inline]
pub fn real_seg_lookup(
    vm: usize,
    seg: i64,
    layout: i64,
    epoch: u64,
    site_kind: u8,
) -> Option<RealSegHit> {
    REAL_SEG_ENTRIES
        .try_with(|cell| {
            let guard = cell.borrow();
            guard.0.iter().flatten().find_map(|e| {
                (e.seg == seg
                    && e.layout == layout
                    && e.epoch == epoch
                    && e.site_kind == site_kind
                    && e.vm == vm)
                    .then_some(e.hit)
            })
        })
        .ok()
        .flatten()
}

/// JIT round 20 wave 8 (lane huntFFM): drop this OS thread's real-segment
/// memo. An entry's `owner_ok` is the confined-owner test against the Java
/// thread that filled it; a carrier that mounts another virtual thread
/// (`vm::jdk_continuation::set_current_thread`) must not reuse it, or the
/// compiled `MemorySegment.get/set` door serves virtual thread V2 a segment
/// confined to V1 instead of `WrongThreadException` (probe
/// `R20HuntFFMVThread`).
pub fn forget_real_seg_entries() {
    let _ = REAL_SEG_ENTRIES.try_with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            guard.0 = [None; REAL_SEG_WAYS];
            guard.1 = 0;
        }
    });
}

pub fn real_seg_install(entry: RealSegEntry) {
    let _ = REAL_SEG_ENTRIES.try_with(|cell| {
        let mut guard = cell.borrow_mut();
        let slot = guard.1;
        guard.0[slot] = Some(entry);
        guard.1 = (slot + 1) % REAL_SEG_WAYS;
    });
}

/// The coordinate of one FFM element access on the cached road: an element
/// index (`getAtIndex`, or an aligned `get(layout, offset)` the emitter
/// converted), or -- JIT r23 wave 4 (lane L8,
/// `CRATONVM_JIT_FFM_UNALIGNED_OFFSET_ACCESS`) -- a raw BYTE offset, for a
/// `get/set(layout, offset)` whose offset is not a multiple of the width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfmCoord {
    Index(i64),
    Byte(i64),
}

impl FfmCoord {
    /// The byte offset this coordinate names for an element of `width`
    /// bytes, or `None` on overflow (a decline).
    #[inline]
    pub fn byte_offset(self, width: i64) -> Option<i64> {
        match self {
            FfmCoord::Index(i) => i.checked_mul(width),
            FfmCoord::Byte(b) => Some(b),
        }
    }
}

/// The word-0 marker of the single-pass FFM block for a byte-offset access
/// (JIT r23 wave 4, lane L8): the emitter stores it in place of the element
/// index when a `get/set(layout, offset)` offset is not a multiple of the
/// width, and the helper then reads the offset as pushed from word 4. Never a
/// converted index: `offset >> log2(width)` with a width of 2 or more is at
/// least `i64::MIN / 2`. Must equal `cratonvm_jit`'s copy.
pub const FFM_BYTE_OFFSET_MARK: i64 = cratonvm_jit::FFM_BYTE_OFFSET_MARK;

/// The site carrier (`B S C I J F D`) of an `FFM_KIND_*` code, or `None`.
#[inline]
pub fn ffm_site_kind(kind: i64) -> Option<u8> {
    Some(match kind {
        FFM_KIND_BYTE => b'B',
        FFM_KIND_SHORT => b'S',
        FFM_KIND_CHAR => b'C',
        FFM_KIND_INT => b'I',
        FFM_KIND_LONG => b'J',
        FFM_KIND_FLOAT => b'F',
        FFM_KIND_DOUBLE => b'D',
        _ => return None,
    })
}

/// JIT r23 wave 5 (lane L8, `CRATONVM_FFM_REAL_MEMO_FIRST`): the hit road of
/// [`ffm_real_segment_access`] alone -- `Some(served)` when this thread's
/// real-segment memo holds `(seg, layout)` for the current epoch, `None` on a
/// miss (the caller then runs the full door: carrier verdicts first, then the
/// real road's fill). An address the memo holds for the current epoch was
/// classified a JDK segment by `segment_layout_facts`, and an object keeps its
/// address and class until the next collection (which bumps the epoch), so it
/// is never a carrier and the carrier probe could not have answered for it.
#[inline]
// SAFETY: `vm_ptr` is live and `seg` is a valid heap pointer or null.
pub unsafe fn ffm_real_memo_serve(
    vm_ptr: i64,
    seg: i64,
    layout_kind: i64,
    coord: FfmCoord,
    write: Option<i64>,
    out: *mut i64,
) -> Option<i64> {
    if vm_ptr == 0 || seg == 0 {
        return None;
    }
    let (layout, kind) = ffm_split_layout_kind(layout_kind);
    if layout == 0 {
        return None;
    }
    let site_kind = ffm_site_kind(kind)?;
    let epoch = cratonvm_native_builtins::ffm_fast::epoch();
    let hit = real_seg_lookup(vm_ptr as usize, seg, layout as i64, epoch, site_kind)?;
    let served = real_seg_serve(&hit, site_kind, coord, write, out);
    cratonvm_native_builtins::ffm_fast::note_fast_consult(served);
    Some(i64::from(served))
}

/// SBR4 lane segacc: the compiled `MemorySegment.get/set/getAtIndex/setAtIndex`
/// door's road for a REAL segment (the JDK's `NativeMemorySegmentImpl` over an
/// `Arena`, mapped file or `ofAddress` window).
///
/// A MISS builds the pair's facts once through a native context
/// ([`cratonvm_native_builtins::lang_invoke::segment_layout_facts`]: the
/// layout's plan, the segment's final fields, the session's `state` slot and
/// whether this thread owns it) and files them per thread for the epoch. A HIT
/// touches no context and no field accessor: the bounds, read-only and alignment
/// checks the JDK makes, then -- inside an access window -- the session's
/// `state` word read raw off its cell (the one thing a close changes, so a close
/// needs no verdict to retire) and the access itself. Anything the facts cannot
/// prove declines (returns 0) to the site's ordinary dispatch, which runs the
/// JDK's bytecode and raises whatever it always raised.
///
/// Returns 1 when served (`*out` holds a load's bits).
///
/// SAFETY: raw words from a live compiled frame; they are dereferenced only
/// through facts filed under the CURRENT epoch for those very addresses (an
/// object address stands for its object between collections, which bump the
/// epoch), and the session cell read happens inside the access window. Nothing
/// here allocates or reaches a safepoint.
/// JIT r23 wave 4 (lane L8): at an explicit [`FfmCoord`] (an element index, or a raw byte offset).
// SAFETY: `vm_ptr` is live and `seg` is a valid heap pointer or null.
pub unsafe fn ffm_real_segment_access(
    vm_ptr: i64,
    seg: i64,
    layout_kind: i64,
    coord: FfmCoord,
    write: Option<i64>,
    out: *mut i64,
) -> i64 {
    if vm_ptr == 0 || seg == 0 {
        return 0;
    }
    let (layout, kind) = ffm_split_layout_kind(layout_kind);
    if layout == 0 {
        return 0;
    }
    let Some(site_kind) = ffm_site_kind(kind) else {
        return 0;
    };
    let epoch = cratonvm_native_builtins::ffm_fast::epoch();
    let vm_id = vm_ptr as usize;
    let hit = match real_seg_lookup(vm_id, seg, layout as i64, epoch, site_kind) {
        Some(h) => h,
        None => match real_seg_fill(vm_ptr, seg, layout as i64, epoch, site_kind) {
            Some(h) => h,
            None => {
                // r23 wave 2d, lane L8: a real HEAP segment has no native
                // facts; serve it through its backing array
                // (`segment_layout_heap_access`) instead of the full dispatch.
                let served = ffm_heap_segment_access(
                    vm_ptr,
                    seg,
                    layout as i64,
                    site_kind,
                    coord,
                    write,
                    out,
                );
                cratonvm_native_builtins::ffm_fast::note_fast_consult(served);
                return i64::from(served);
            }
        },
    };
    let served = real_seg_serve(&hit, site_kind, coord, write, out);
    // The engagement counter the exit line prints (`ffm_fast::exit_summary`):
    // a served real-segment access is a hit, a decline a miss.
    cratonvm_native_builtins::ffm_fast::note_fast_consult(served);
    i64::from(served)
}

/// r23 wave 2d, lane L8 (`docs/feature-designs/real-jdk-mode.md`):
/// the heap road of [`ffm_real_segment_access`] -- a real heap segment
/// (`MemorySegment.ofArray`), which [`real_seg_fill`] declines, served through
/// its backing array by
/// `cratonvm_native_builtins::lang_invoke::segment_layout_heap_access` (the
/// JDK's checks in its order, any failure a decline). Writes `*out` for a read.
/// Allocates nothing and reaches no safepoint, so the forwarded references stay
/// current. `false` declines (the site's unchanged dispatch runs).
#[cold]
#[inline(never)]
// SAFETY: `vm_ptr` is live and `seg` is a valid heap pointer or null.
pub unsafe fn ffm_heap_segment_access(
    vm_ptr: i64,
    seg: i64,
    layout: i64,
    site_kind: u8,
    coord: FfmCoord,
    write: Option<i64>,
    out: *mut i64,
) -> bool {
    let vm = &*(vm_ptr as *const SharedVm);
    let Some((seg_class, _)) = direct_receiver_facts(vm, seg as usize) else {
        return false;
    };
    let Some((layout_class, _)) = direct_receiver_facts(vm, layout as usize) else {
        return false;
    };
    if seg_class == 0 || layout_class == 0 {
        return false;
    }
    let Some((thread, _guard)) = jit_thread_mut() else {
        return false;
    };
    let heap = &vm.mem.heap;
    let seg_ref = heap.load_and_forward(ObjectRef::from_raw(seg as usize as *mut u8));
    let layout_ref = heap.load_and_forward(ObjectRef::from_raw(layout as usize as *mut u8));
    let mut ctx = crate::vm::NativeContextImpl {
        shared: vm,
        thread: &mut *thread,
    };
    let answer = match coord {
        FfmCoord::Index(index) => cratonvm_native_builtins::lang_invoke::segment_layout_heap_access(
            &mut ctx, seg_ref, layout_ref, site_kind, index, write,
        ),
        FfmCoord::Byte(offset) => {
            cratonvm_native_builtins::lang_invoke::segment_layout_heap_access_bytes(
                &mut ctx, seg_ref, layout_ref, site_kind, offset, write,
            )
        }
    };
    match answer {
        Some(bits) => {
            if write.is_none() {
                *out = bits;
            }
            true
        }
        None => false,
    }
}

/// The miss road of [`ffm_real_segment_access`]: build and file the facts.
#[cold]
#[inline(never)]
// SAFETY: `vm_ptr` is live and `seg` is a valid heap pointer or null.
pub unsafe fn real_seg_fill(
    vm_ptr: i64,
    seg: i64,
    layout: i64,
    epoch: u64,
    site_kind: u8,
) -> Option<RealSegHit> {
    let vm = &*(vm_ptr as *const SharedVm);
    let (seg_class, _) = direct_receiver_facts(vm, seg as usize)?;
    let (layout_class, _) = direct_receiver_facts(vm, layout as usize)?;
    if seg_class == 0 || layout_class == 0 {
        return None;
    }
    let (thread, _guard) = jit_thread_mut()?;
    let heap = &vm.mem.heap;
    let seg_ref = heap.load_and_forward(ObjectRef::from_raw(seg as usize as *mut u8));
    let layout_ref = heap.load_and_forward(ObjectRef::from_raw(layout as usize as *mut u8));
    let mut ctx = crate::vm::NativeContextImpl {
        shared: vm,
        thread: &mut *thread,
    };
    let facts = cratonvm_native_builtins::lang_invoke::segment_layout_facts(
        &mut ctx, seg_ref, layout_ref, seg_class, site_kind,
    )?;
    // The session's `state` is read raw: where it lives depends on the
    // instance's layout (compact: a tagless `int` at a displacement; legacy: a
    // 16-byte `Value` cell). A compact instance whose layout does not resolve
    // declines.
    let (state_off, state_compact) =
        match jit_compact_field_slot(facts.scope as i64, facts.state_slot as i64) {
            Some((off, cratonvm_types::FieldStorageKind::Int)) => (off, true),
            Some(_) => return None,
            None => {
                let flags = std::ptr::read(
                    (facts.scope as *const u8).add(cratonvm_types::GC_FLAGS_BYTE_OFFSET),
                );
                if flags & cratonvm_types::GC_FLAG_COMPACT != 0 {
                    return None;
                }
                (HEADER_SIZE + facts.state_slot * SLOT_SIZE, false)
            }
        };
    let hit = RealSegHit {
        facts,
        state_off,
        state_compact,
    };
    real_seg_install(RealSegEntry {
        vm: vm_ptr as usize,
        seg,
        layout,
        epoch,
        site_kind,
        hit,
    });
    Some(hit)
}

/// The hit road of [`ffm_real_segment_access`]: the JDK's checks in the JDK's
/// order, then the access. `false` declines.
#[inline]
// SAFETY: `hit` is valid for this access window and `seg` is a live segment.
pub unsafe fn real_seg_serve(
    hit: &RealSegHit,
    site_kind: u8,
    coord: FfmCoord,
    write: Option<i64>,
    out: *mut i64,
) -> bool {
    let f = &hit.facts;
    // JIT r23 wave 4 (lane L8): an index or a raw byte offset; the checks
    // below are the same for both, in the same order (bounds, read-only,
    // then the layout's alignment against `min + offset`).
    let Some(base) = coord.byte_offset(f.width) else {
        return false;
    };
    if base < 0 || base.saturating_add(f.width) > f.size || (write.is_some() && f.read_only) {
        return false;
    }
    if f.align > 1 && f.min.wrapping_add(base) & (f.align - 1) != 0 {
        return false;
    }
    let Some(addr) = f.min.checked_add(base) else {
        return false;
    };
    // The access window: from before the session check to after the last touch
    // of the block (see `ffm_fast::AccessWindow`).
    let window = cratonvm_native_builtins::ffm_fast::AccessWindow::open();
    if !window.admits_fast_path() || !f.owner_ok {
        return false;
    }
    let state = if hit.state_compact {
        cratonvm_types::read_compact_field(
            (f.scope as *const u8).add(hit.state_off),
            cratonvm_types::FieldStorageKind::Int,
            std::sync::atomic::Ordering::Acquire,
        )
    } else {
        jit_read_value_cell_checked(
            (f.scope as *const u8).add(hit.state_off) as *const Value,
            "real_seg_serve",
        )
    };
    match state {
        Value::Int(state) if state >= 0 => {}
        _ => return false,
    }
    let width = f.width as usize;
    let swap = |raw: u64| -> u64 {
        match width {
            1 => raw,
            2 => (raw as u16).swap_bytes() as u64,
            4 => (raw as u32).swap_bytes() as u64,
            _ => raw.swap_bytes(),
        }
    };
    match write {
        Some(bits) => {
            let mask = if width == 8 {
                u64::MAX
            } else {
                (1u64 << (width * 8)) - 1
            };
            let mut raw = (bits as u64) & mask;
            if f.be {
                raw = swap(raw);
            }
            // JIT r23 wave 5 (lane L8, `CRATONVM_FFM_ARENA_JIT_RESOLVE`): a
            // slotted arena handle resolves through the lock-free JIT table to
            // the block's real bytes; a miss keeps the locked copy below.
            let resolved = if f.handle {
                cratonvm_native_builtins::unsafe_arena_jit_resolve(addr, width as u64)
            } else {
                None
            };
            if let Some(real) = resolved {
                // SAFETY: `real` is the live block's own byte for `addr`, with
                // `width` bytes inside the block (the table's length test),
                // read inside the access window after the session check, so a
                // close's free waits for this store (see
                // `unsafe_arena_jit_resolve`).
                let ptr = real as *mut u8;
                match width {
                    1 => *ptr = raw as u8,
                    2 => (ptr as *mut u16).write_unaligned(raw as u16),
                    4 => (ptr as *mut u32).write_unaligned(raw as u32),
                    _ => (ptr as *mut u64).write_unaligned(raw),
                }
                true
            } else if f.handle {
                let bytes = raw.to_le_bytes();
                cratonvm_native_builtins::unsafe_arena_copy_in(addr, &bytes[..width])
            } else {
                // SAFETY: the address is the segment's own `min + offset`, inside
                // its bounds (checked above), as `segment_native_set`.
                let ptr = addr as usize as *mut u8;
                match width {
                    1 => *ptr = raw as u8,
                    2 => (ptr as *mut u16).write_unaligned(raw as u16),
                    4 => (ptr as *mut u32).write_unaligned(raw as u32),
                    _ => (ptr as *mut u64).write_unaligned(raw),
                }
                true
            }
        }
        None => {
            // As the store above.
            let resolved = if f.handle {
                cratonvm_native_builtins::unsafe_arena_jit_resolve(addr, width as u64)
            } else {
                None
            };
            let raw = if let Some(real) = resolved {
                // SAFETY: as the store above.
                let ptr = real as *const u8;
                match width {
                    1 => *ptr as u64,
                    2 => (ptr as *const u16).read_unaligned() as u64,
                    4 => (ptr as *const u32).read_unaligned() as u64,
                    _ => (ptr as *const u64).read_unaligned(),
                }
            } else if f.handle {
                let mut buf = [0u8; 8];
                if !cratonvm_native_builtins::unsafe_arena_copy_out(addr, &mut buf[..width]) {
                    return false;
                }
                u64::from_le_bytes(buf)
            } else {
                // SAFETY: as the store above; `segment_native_get`'s own read.
                let ptr = addr as usize as *const u8;
                match width {
                    1 => *ptr as u64,
                    2 => (ptr as *const u16).read_unaligned() as u64,
                    4 => (ptr as *const u32).read_unaligned() as u64,
                    _ => (ptr as *const u64).read_unaligned(),
                }
            };
            let raw = if f.be { swap(raw) } else { raw };
            *out = match site_kind {
                b'B' => raw as u8 as i8 as i64,
                b'S' => raw as u16 as i16 as i64,
                b'C' => raw as u16 as i64,
                b'I' => raw as u32 as i32 as i64,
                b'F' => raw as u32 as i64,
                _ => raw as i64,
            };
            true
        }
    }
}

/// JIT round 23 wave 3, lane L8 (`CRATONVM_JIT_IR_FFM_ELEMENT_DIRECT`): the
/// element kind and index of one optimizing-tier `MemorySegment` element
/// site, from its judged record's descriptor and method: the `FFM_KIND_*` code
/// and the element index the cached road takes (a `get/set(layout, long
/// offset)` site converts a byte offset that is a multiple of the width; any
/// other offset is `None`, a decline). `None` for a carrier the road does not
/// serve.
#[cfg(test)]
pub fn ffm_element_site_kind_index(info: &JitInvokeInfo, index: i64) -> Option<(i64, i64)> {
    ffm_element_word_kind_index(
        cratonvm_jit::ffm_element_site_word(info.class_name, info.method_name, info.descriptor),
        index,
    )
}

/// [`ffm_element_site_kind_index`] from the site's precomputed word
/// (`VarHandleJudgedSite::ffm_element`, JIT round 23 wave 4): the helper's
/// per-call form. Re-parsing the descriptor and method name on every call was
/// 27% of the bound route's time (RIP sampling, n23-w3: `memchr`,
/// `starts_with`, `memcmp`, `ffm_kind_for_descriptor`).
#[inline]
pub fn ffm_element_word_kind_index(word: u32, index: i64) -> Option<(i64, i64)> {
    if word & cratonvm_jit::FFM_ELEMENT_SITE_VALID == 0 {
        return None;
    }
    let kind = i64::from(word & 0xFF);
    let shift = match kind {
        FFM_KIND_BYTE => 0,
        FFM_KIND_SHORT | FFM_KIND_CHAR => 1,
        FFM_KIND_INT | FFM_KIND_FLOAT => 2,
        FFM_KIND_LONG | FFM_KIND_DOUBLE => 3,
        _ => return None,
    };
    let index = if word & cratonvm_jit::FFM_ELEMENT_SITE_BYTE_OFFSET != 0 {
        if index & ((1i64 << shift) - 1) != 0 {
            return None;
        }
        index >> shift
    } else {
        index
    };
    Some((kind, index))
}

/// The cached road's coordinate for one optimizing-tier element access
/// (JIT r23 wave 4): [`ffm_element_word_kind_index`]'s element index, or --
/// for a byte-offset site whose offset is not a multiple of the width and
/// whose word carries `FFM_ELEMENT_SITE_UNALIGNED_OK` -- the raw byte offset.
#[inline]
pub fn ffm_element_word_coord(word: u32, index: i64) -> Option<(i64, FfmCoord)> {
    if let Some((kind, element)) = ffm_element_word_kind_index(word, index) {
        return Some((kind, FfmCoord::Index(element)));
    }
    if word & cratonvm_jit::FFM_ELEMENT_SITE_VALID == 0
        || word & cratonvm_jit::FFM_ELEMENT_SITE_BYTE_OFFSET == 0
        || word & cratonvm_jit::FFM_ELEMENT_SITE_UNALIGNED_OK == 0
    {
        return None;
    }
    let kind = i64::from(word & 0xFF);
    (0..=6).contains(&kind).then_some((kind, FfmCoord::Byte(index)))
}

/// JIT round 23 wave 3, lane L8 (`CRATONVM_JIT_IR_FFM_ELEMENT_DIRECT`;
/// `docs/feature-designs/real-jdk-mode.md`):
/// the optimizing tier's thin `MemorySegment.getAtIndex` / `get(layout, long)`
/// for an integral or floating carrier. `site` is the JUDGED site record
/// (`cratonvm_jit::VarHandleJudgedSite`, tagged) the IR binding bakes: its
/// `info` is the site's own dispatch row. The access is served on the FFM
/// element door's cached road ([`ffm_real_segment_access`]: native and, under
/// `CRATONVM_FFM_SEGMENT_LAYOUT_HEAP_FACTS`, heap segments; the JDK's checks in
/// its order); any decline dispatches the site exactly as an unbound call
/// would (`jit_invoke_dispatch` with the real row), so every exception, frame
/// and answer is the JDK's. Returns the value's bits (a float's / double's raw
/// bits) or the dispatch's word (`i64::MIN`: thrown).
pub unsafe extern "C" fn jit_ffm_element_get_direct(
    site: i64,
    seg: i64,
    layout: i64,
    index: i64,
) -> i64 {
    // JIT round 23 wave 4 (lane L8): the site's kind and coordinate form come
    // from the record's precomputed word, and the hand-off drop runs only after
    // a dispatch -- the served arm calls no native and returns a primitive, as
    // the single-pass `jit_ffm_segment_get` serving the same road does.
    let (vm_ptr, info, word) = ffm_element_judged_site(site);
    let (bits, dispatched) = contain(
        "jit_ffm_element_get_direct",
        OnPanic::Throw { vm_ptr },
        (i64::MIN, true),
        || {
            if layout != 0 && layout & FFM_KIND_MASK == 0 {
                if let Some((kind, coord)) = ffm_element_word_coord(word, index) {
                    let mut out: i64 = 0;
                    if ffm_real_segment_access(vm_ptr, seg, layout | kind, coord, None, &mut out)
                        != 0
                    {
                        return (out, false);
                    }
                }
            }
            crate::jit::conservative_roots::note_jit_boundary();
            assert!(!info.is_null(), "an FFM element direct bind carries a judged site");
            let args = [seg, layout, index];
            (
                jit_invoke_dispatch(vm_ptr, info as i64, args.as_ptr() as i64, 3),
                true,
            )
        },
    );
    if dispatched {
        native_return_handed_to_compiled_code();
    }
    bits
}

/// The judged site of an FFM element direct bind: `(vm_ptr, info, the
/// record's ffm_element word)`, or `(word, null, 0)` for an untagged word
/// (never baked for these helpers; the dispatch arm then asserts).
///
/// # Safety
/// As [`varhandle_thin_call_site`].
#[inline]
pub unsafe fn ffm_element_judged_site(word: i64) -> (i64, *const JitInvokeInfo, u32) {
    // Cast: the JIT ABI word is an address.
    let w = word as usize;
    if w & cratonvm_jit::VARHANDLE_JUDGED_SITE_TAG == 0 {
        return (word, std::ptr::null(), 0);
    }
    // SAFETY: as in `varhandle_thin_call_site`.
    let site = &*((w & !cratonvm_jit::VARHANDLE_JUDGED_SITE_TAG)
        as *const cratonvm_jit::VarHandleJudgedSite);
    // Cast: the record's `SharedVm` address back to the JIT ABI word.
    (
        site.vm_ptr as i64,
        &site.info as *const JitInvokeInfo,
        site.ffm_element,
    )
}

/// The write twin of [`jit_ffm_element_get_direct`]: `setAtIndex` / `set(layout,
/// long, value)`, `value` the carrier's bits. Returns `0`, or the dispatch's
/// word (`i64::MIN`: thrown).
pub unsafe extern "C" fn jit_ffm_element_set_direct(
    site: i64,
    seg: i64,
    layout: i64,
    index: i64,
    value: i64,
) -> i64 {
    // As in `jit_ffm_element_get_direct`.
    let (vm_ptr, info, word) = ffm_element_judged_site(site);
    let (bits, dispatched) = contain(
        "jit_ffm_element_set_direct",
        OnPanic::Throw { vm_ptr },
        (i64::MIN, true),
        || {
            if layout != 0 && layout & FFM_KIND_MASK == 0 {
                if let Some((kind, coord)) = ffm_element_word_coord(word, index) {
                    let mut out: i64 = 0;
                    if ffm_real_segment_access(
                        vm_ptr,
                        seg,
                        layout | kind,
                        coord,
                        Some(value),
                        &mut out,
                    ) != 0
                    {
                        return (0, false);
                    }
                }
            }
            crate::jit::conservative_roots::note_jit_boundary();
            assert!(!info.is_null(), "an FFM element direct bind carries a judged site");
            let args = [seg, layout, index, value];
            (
                jit_invoke_dispatch(vm_ptr, info as i64, args.as_ptr() as i64, 4),
                true,
            )
        },
    );
    if dispatched {
        native_return_handed_to_compiled_code();
    }
    bits
}



/// FFM element READ fast path. Returns 1 and writes `*out` when handled, 0 to
/// decline (the caller then runs the unchanged native dispatch).
///
/// # Safety
/// Called from JIT-compiled code. `seg` is an object reference or 0;
/// `layout_kind` is the layout reference (or 0) with the `FFM_KIND_*` code in
/// its low three bits (see [`FFM_KIND_MASK`]); `out` must point to a writable
/// `i64`.
pub unsafe extern "C" fn jit_ffm_segment_get(
    vm_ptr: i64,
    seg: i64,
    layout_kind: i64,
    block: *mut i64,
) -> i64 {
    let f = || jit_ffm_segment_get_body(vm_ptr, seg, layout_kind, block);
    contain("jit_ffm_segment_get", OnPanic::RecordFor { vm_ptr }, 0, f)
}

/// Body of [`jit_ffm_segment_get`], run under its panic guard.
///
/// SBR4 lane segacc: the call site hands `(vm_ptr, segment, layout | kind,
/// block)` where `block[0]` is the element index and `block[1]` receives the
/// value. A validated carrier takes the verdict road below; anything else is
/// offered to [`ffm_real_segment_access`], which serves the JDK's own native
/// segment (the carriers are never minted once the segment rows are retired).
pub unsafe fn jit_ffm_segment_get_body(
    vm_ptr: i64,
    seg: i64,
    layout_kind: i64,
    block: *mut i64,
) -> i64 {
    let index = *block;
    let out = block.add(1);
    // JIT r23 wave 4 (lane L8): an unaligned `get(layout, offset)`; the
    // offset as pushed is word 4. The carrier road takes element indices
    // only, so it declines such an access (the dispatch serves it).
    let coord = if index == FFM_BYTE_OFFSET_MARK {
        FfmCoord::Byte(*block.add(4))
    } else {
        FfmCoord::Index(index)
    };
    if let Some(served) = ffm_real_memo_serve(vm_ptr, seg, layout_kind, coord, None, out) {
        return served;
    }
    if cratonvm_types::plausible_heap_pointer(seg as u64)
        && cratonvm_native_builtins::ffm_fast::is_validated(seg as u64, false)
    {
        let FfmCoord::Index(index) = coord else {
            return 0;
        };
        return ffm_carrier_get_body(seg, index, layout_kind, out);
    }
    ffm_real_segment_access(vm_ptr, seg, layout_kind, coord, None, out)
}

/// The carrier verdict road of [`jit_ffm_segment_get`].
pub unsafe fn ffm_carrier_get_body(seg: i64, index: i64, layout_kind: i64, out: *mut i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache.
    crate::jit::conservative_roots::note_jit_boundary();
    let (layout, kind) = ffm_split_layout_kind(layout_kind);
    let Some(width) = ffm_kind_width(kind) else {
        cratonvm_native_builtins::ffm_fast::note_fast_consult(false);
        return 0;
    };
    // The close handshake (round 12 wave 3, lane ffm): open from before the
    // verdict's epoch read until after the load, so a shared-arena close on
    // another thread either waits for this access or retires the verdict it
    // would have used. See `ffm_fast::AccessWindow`.
    let window = cratonvm_native_builtins::ffm_fast::AccessWindow::open();
    if !window.admits_fast_path() {
        cratonvm_native_builtins::ffm_fast::note_fast_consult(false);
        return 0;
    }
    let Some((base, size)) = ffm_resolve_carrier(seg, false) else {
        cratonvm_native_builtins::ffm_fast::note_fast_consult(false);
        return 0;
    };
    // The layout: a host-order value layout the native has read (a
    // `BIG_ENDIAN` one never has a verdict), with its alignment mask.
    let Some(align_mask) = ffm_layout_align_mask(layout) else {
        cratonvm_native_builtins::ffm_fast::note_fast_consult(false);
        return 0;
    };
    let Some(offset) = ffm_checked_offset(index, width, size) else {
        cratonvm_native_builtins::ffm_fast::note_fast_consult(false);
        return 0;
    };
    let addr = base + offset as u64;
    // A misaligned absolute address is the native's
    // `IllegalArgumentException: Misaligned access`, so decline to it.
    if addr & align_mask != 0 {
        cratonvm_native_builtins::ffm_fast::note_fast_consult(false);
        return 0;
    }
    // SAFETY: `addr` lies inside the carrier's block — `base` came from the
    // carrier's own address slots and `offset + width <= size` was just checked
    // — and the block is live, which is what the verdict asserts.
    //
    // Widths and signedness match `pe_segment_get_impl` exactly: byte and short
    // sign-extend, char zero-extends, float and double return raw bits for the
    // caller to move into an XMM register.
    let value: i64 = match kind {
        FFM_KIND_BYTE => i64::from(*(addr as *const i8)),
        FFM_KIND_SHORT => i64::from(std::ptr::read_unaligned(addr as *const i16)),
        FFM_KIND_CHAR => i64::from(std::ptr::read_unaligned(addr as *const u16)),
        FFM_KIND_INT => i64::from(std::ptr::read_unaligned(addr as *const i32)),
        FFM_KIND_LONG => std::ptr::read_unaligned(addr as *const i64),
        FFM_KIND_FLOAT => i64::from(std::ptr::read_unaligned(addr as *const u32)),
        FFM_KIND_DOUBLE => std::ptr::read_unaligned(addr as *const i64),
        _ => {
            cratonvm_native_builtins::ffm_fast::note_fast_consult(false);
            return 0;
        }
    };
    *out = value;
    cratonvm_native_builtins::ffm_fast::note_fast_consult(true);
    1
}

/// FFM element WRITE fast path. Returns 1 when handled, 0 to decline.
///
/// `value` carries the raw bits: the integral kinds in their low bytes, float
/// and double as `to_bits()`, matching what `pe_segment_set_impl` writes.
///
/// # Safety
/// Called from JIT-compiled code. `seg` is an object reference or 0;
/// `layout_kind` is as for [`jit_ffm_segment_get`].
pub unsafe extern "C" fn jit_ffm_segment_set(
    vm_ptr: i64,
    seg: i64,
    layout_kind: i64,
    block: i64,
) -> i64 {
    let f = || jit_ffm_segment_set_body(vm_ptr, seg, layout_kind, block as *mut i64);
    contain("jit_ffm_segment_set", OnPanic::RecordFor { vm_ptr }, 0, f)
}

/// Body of [`jit_ffm_segment_set`], run under its panic guard (`block[0]` the
/// element index, `block[1]` the raw value bits).
pub unsafe fn jit_ffm_segment_set_body(
    vm_ptr: i64,
    seg: i64,
    layout_kind: i64,
    block: *mut i64,
) -> i64 {
    let index = *block;
    let value = *block.add(1);
    // As in `jit_ffm_segment_get_body`.
    let coord = if index == FFM_BYTE_OFFSET_MARK {
        FfmCoord::Byte(*block.add(4))
    } else {
        FfmCoord::Index(index)
    };
    let mut unused = 0i64;
    if let Some(served) =
        ffm_real_memo_serve(vm_ptr, seg, layout_kind, coord, Some(value), &mut unused)
    {
        return served;
    }
    if cratonvm_types::plausible_heap_pointer(seg as u64)
        && cratonvm_native_builtins::ffm_fast::is_validated(seg as u64, true)
    {
        let FfmCoord::Index(index) = coord else {
            return 0;
        };
        return ffm_carrier_set_body(seg, index, layout_kind, value);
    }
    ffm_real_segment_access(vm_ptr, seg, layout_kind, coord, Some(value), &mut unused)
}

/// The carrier verdict road of [`jit_ffm_segment_set`].
pub unsafe fn ffm_carrier_set_body(seg: i64, index: i64, layout_kind: i64, value: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    let (layout, kind) = ffm_split_layout_kind(layout_kind);
    let Some(width) = ffm_kind_width(kind) else {
        cratonvm_native_builtins::ffm_fast::note_fast_consult(false);
        return 0;
    };
    // The close handshake: see `jit_ffm_segment_get_body`.
    let window = cratonvm_native_builtins::ffm_fast::AccessWindow::open();
    if !window.admits_fast_path() {
        cratonvm_native_builtins::ffm_fast::note_fast_consult(false);
        return 0;
    }
    // `want_write`: only a carrier the native has actually completed a write on
    // is eligible, so a read-only carrier can never be written here.
    let Some((base, size)) = ffm_resolve_carrier(seg, true) else {
        cratonvm_native_builtins::ffm_fast::note_fast_consult(false);
        return 0;
    };
    // Byte order and alignment: see `jit_ffm_segment_get_body`.
    let Some(align_mask) = ffm_layout_align_mask(layout) else {
        cratonvm_native_builtins::ffm_fast::note_fast_consult(false);
        return 0;
    };
    let Some(offset) = ffm_checked_offset(index, width, size) else {
        cratonvm_native_builtins::ffm_fast::note_fast_consult(false);
        return 0;
    };
    let addr = base + offset as u64;
    if addr & align_mask != 0 {
        cratonvm_native_builtins::ffm_fast::note_fast_consult(false);
        return 0;
    }
    // SAFETY: as `jit_ffm_segment_get`, plus a verdict that the native
    // completed a WRITE to this carrier at this epoch.
    match kind {
        FFM_KIND_BYTE => std::ptr::write_unaligned(addr as *mut u8, value as u8),
        FFM_KIND_SHORT | FFM_KIND_CHAR => std::ptr::write_unaligned(addr as *mut u16, value as u16),
        FFM_KIND_INT | FFM_KIND_FLOAT => std::ptr::write_unaligned(addr as *mut u32, value as u32),
        FFM_KIND_LONG | FFM_KIND_DOUBLE => std::ptr::write_unaligned(addr as *mut i64, value),
        _ => {
            cratonvm_native_builtins::ffm_fast::note_fast_consult(false);
            return 0;
        }
    }
    cratonvm_native_builtins::ffm_fast::note_fast_consult(true);
    1
}


/// NIO19-2 (nio20 wave 1, lane L2): the VM's compile-time answer for
/// `cratonvm_jit::DirectHelperTable::unsafe_inline_admitted` -- bit `i` set
/// when the `i`th row of `cratonvm_jit::direct_helpers::UnsafeInlineAccess::GATE_BITS`
/// may be answered by inline code that asks no policy at all.
///
/// That is sound exactly when the per-call policy the inline code skips could
/// only admit:
///
/// * the capability half: `crate::vm::vm_exec::check_native_dispatch_capability_for_slot`
///   can only answer `Ok(())` when this VM has no capability policy, or
///   the policy is `Permissive` -- which is EVERY VM by default: `vm_init`
///   installs `CapabilitySet::from_env`, `Permissive` unless
///   `CRATONVM_CAPABILITY_MODE` says otherwise (an orchestrator run of the
///   first cut, which cleared every bit for any installed policy, measured a
///   mask of 0 on a default VM, i.e. the inline path never engaged).
///   `Permissive` allows everything and only RECORDS the distinct
///   `(RawMemory, Any)` use, memoised per thread after the first; that record
///   is made here, once per compile, through the same function, so a body
///   whose only `Unsafe` traffic is inline still shows the use in the audit
///   report. `Audit` (which evaluates every request) and `Enforce` (which may
///   deny) clear every bit, and those sites keep their calls;
/// * the `--real-jdk` half: [`direct_helper_tail_would_refuse`] for the row's
///   gate, the same predicate the thin helper's per-call gate and the bind
///   admission share. Its inputs (the registered kind of an `ACC_NATIVE`
///   triple, the memoised `CRATONVM_ENFORCE_NATIVE_SHADOW`) do not change
///   under a running VM. Under `--compatible` it is the capability half alone.
///
/// Compile-time only. Per VM, never cached: the table is built per compile.
/// nio21 wave 7 L3 (W6-L3-2): admission bits 16 and 17 of
/// `DirectHelperTable::unsafe_inline_admitted`, the `compareAndExchange{Int,Long}`
/// rows (`UnsafeInlineRmw::CAE_GATE_BITS`). The question
/// `direct_helper_refusal` asks, with the per-VM memo
/// `JitRealm::unsafe_inline.cae_sites[i]` in place of a static gate (these rows
/// have no thin helper, and AGENTS.md forbids new process statics for per-VM
/// state). Compile-time only.
pub fn unsafe_cae_admitted_bits(shared: &crate::vm::SharedVm) -> usize {
    use cratonvm_jit::direct_helpers::UnsafeInlineRmw;
    const CLASS: &str = "jdk/internal/misc/Unsafe";
    let registry = &shared.natives.native_methods;
    let mut bits = 0usize;
    for (i, (method, desc)) in UnsafeInlineRmw::CAE_GATE_BITS.iter().enumerate() {
        let id = shared.jit.unsafe_inline.cae_sites[i].resolve(registry, CLASS, method, desc);
        let kind = id.and_then(|id| registry.kind_of_id(id));
        let refused = direct_helper_refusal(
            shared,
            CLASS,
            method,
            desc,
            Some((
                cratonvm_native_api::CapabilityKind::RawMemory,
                CapabilityQuestion::Call(id),
            )),
            kind,
        );
        if !refused {
            bits |= 1 << (UnsafeInlineRmw::CAE_FIRST_BIT + i);
        }
    }
    bits
}

/// JIT round 20 wave 11, lane pAtom (W6-L3-3 / AT-5): the
/// `DirectHelperTable::unsafe_inline_admitted` word [`direct_helper_table_for`]
/// bakes into a compile, for the inline resolver's `Unsafe` RMW splice veto
/// (`jit_bridge::resolve_inline_site_from`,
/// `cratonvm_jit::direct_helpers::unsafe_inline_rmw_body_splice_loses_prefix`),
/// which must ask the same word the prefix will. Compile-time only.
pub fn unsafe_inline_admitted_word_for_compile(shared: &crate::vm::SharedVm) -> usize {
    let mask = unsafe_inline_admitted_mask(shared);
    if mask != 0 {
        let mut word = mask
            | unsafe_cae_admitted_bits(shared)
            | unsafe_qu_admitted_bits(shared)
            | unsafe_subword_admitted_bits(shared);
        // JIT round 21 wave 1, lane updaters (UPDATERS21-2): whether this
        // compile's RMW prefix has a field arm, for the AT-5 splice veto only
        // (never part of the table's word).
        if unsafe_field_cells_would_bake(shared) {
            word |= 1 << cratonvm_jit::direct_helpers::UNSAFE_FIELD_ARM_BAKED_BIT;
        }
        word
    } else {
        mask
    }
}

/// JIT round 21 wave 1, lane updaters: would the table fill of this VM bake
/// an `Unsafe` instance-field cell table (`table.unsafe_field_cells != 0`)?
/// The fill's own rule, asked without arming the publisher.
pub fn unsafe_field_cells_would_bake(shared: &crate::vm::SharedVm) -> bool {
    (!shared.classes.unsafe_byte_field_offsets
        || cratonvm_jit::direct_helpers::unsafe_field_cells_byte_offsets_enabled())
        && shared.jit.unsafe_inline.would_bake_for_compile()
}

/// JIT round 21, lane atomics (UNSAFE21-2): admission bits 29..33 of
/// `DirectHelperTable::unsafe_inline_admitted`, the sub-word CAS rows
/// (`cratonvm_jit::direct_helpers::UnsafeSubwordRmw::GATE_BITS`), asked like
/// [`unsafe_cae_admitted_bits`] (the same `direct_helper_refusal` question
/// with the per-VM memos `JitRealm::unsafe_inline.subword_sites`; these
/// natives have no thin helper) and only under the JIT's
/// `CRATONVM_JIT_UNSAFE_SUBWORD_RMW_INLINE`. Compile-time only.
pub fn unsafe_subword_admitted_bits(shared: &crate::vm::SharedVm) -> usize {
    use cratonvm_jit::direct_helpers::UnsafeSubwordRmw;
    const CLASS: &str = "jdk/internal/misc/Unsafe";
    if !cratonvm_jit::direct_helpers::unsafe_subword_rmw_inline_enabled() {
        return 0;
    }
    let registry = &shared.natives.native_methods;
    let mut bits = 0usize;
    for (i, (method, desc)) in UnsafeSubwordRmw::GATE_BITS.iter().enumerate() {
        let id = shared.jit.unsafe_inline.subword_sites[i].resolve(registry, CLASS, method, desc);
        let kind = id.and_then(|id| registry.kind_of_id(id));
        let refused = direct_helper_refusal(
            shared,
            CLASS,
            method,
            desc,
            Some((
                cratonvm_native_api::CapabilityKind::RawMemory,
                CapabilityQuestion::Call(id),
            )),
            kind,
        );
        if !refused {
            bits |= 1 << (UnsafeSubwordRmw::FIRST_BIT + i);
        }
    }
    bits
}

/// JIT round 20 wave 12, lane qUnsafe: admission bits 18..25 of
/// `DirectHelperTable::unsafe_inline_admitted`, asked like
/// [`unsafe_cae_admitted_bits`] (the same `direct_helper_refusal` question,
/// per VM, compile time only) and only where the capability half admitted
/// anything at all:
///
/// * bit 18 (`cratonvm_jit::direct_helpers::UNSAFE_REF_CAS_GATE_BIT`, AT-4):
///   `compareAndSetReference`, through its thin helper's own gate
///   (`UNSAFE_CAS_REFERENCE_GATE`, whose memo is already per VM); only under
///   the JIT's `CRATONVM_JIT_UNSAFE_QU_REF_CAS_FIELD`;
/// * bits 19..25 (`UNSAFE_QU_EXTRA_GATE_BITS`, L2-2): the `boolean` /
///   `float` / `double` accessor natives, through the per-VM memos
///   `JitRealm::unsafe_inline.qu_extra_sites` (no thin helper, so no gate);
///   only under `CRATONVM_JIT_UNSAFE_QU_EXTRA_ROWS`.
pub fn unsafe_qu_admitted_bits(shared: &crate::vm::SharedVm) -> usize {
    use cratonvm_jit::direct_helpers as dh;
    const CLASS: &str = "jdk/internal/misc/Unsafe";
    let mut bits = 0usize;
    if dh::unsafe_qu_ref_cas_field_enabled()
        && !direct_helper_tail_would_refuse(shared, &UNSAFE_CAS_REFERENCE_GATE)
    {
        bits |= 1 << dh::UNSAFE_REF_CAS_GATE_BIT;
    }
    // JIT round 21 wave 1, lane updaters (UPDATERS21-1, opt-in): the
    // reference volatile get / put field arm, through the thin helpers' gates.
    if dh::unsafe_ref_volatile_field_enabled() {
        if !direct_helper_tail_would_refuse(shared, &UNSAFE_GET_REFERENCE_VOLATILE_GATE) {
            bits |= 1 << dh::UNSAFE_REF_VOLATILE_GET_GATE_BIT;
        }
        if !direct_helper_tail_would_refuse(shared, &UNSAFE_PUT_REFERENCE_VOLATILE_GATE) {
            bits |= 1 << dh::UNSAFE_REF_VOLATILE_PUT_GATE_BIT;
        }
    }
    // JIT round 23 wave 1, lane L9 (`CRATONVM_JIT_UNSAFE_REF_GET_AND_SET_FIELD`,
    // default ON): the `getAndSetReference*` field arm, admitted when neither
    // native its JDK body calls (`getReferenceVolatile`,
    // `compareAndSetReference`) would be refused by its thin helper's gate.
    // JIT round 21 wave 1, lane updaters (review UPD21-R2): and only where the
    // heap publishes a reference-store gate plan. The arm stores through the
    // gated reference-store screen, so a collector with none (G1) never gets
    // the arm; the AT-5 splice veto reads this bit, and a set bit there kept
    // the `getAndSetReference` Java body a CALL on a heap whose compile
    // serves it with neither the arm nor the spliced loop.
    if dh::unsafe_ref_get_and_set_field_enabled()
        && ref_store_plan_words_publish_gates(ref_store_plan_for_heap(Some(&shared.mem.heap)))
        && !direct_helper_tail_would_refuse(shared, &UNSAFE_CAS_REFERENCE_GATE)
        && !direct_helper_tail_would_refuse(shared, &UNSAFE_GET_REFERENCE_VOLATILE_GATE)
    {
        bits |= 1 << dh::UNSAFE_REF_GET_AND_SET_GATE_BIT;
    }
    if dh::unsafe_qu_extra_rows_enabled() {
        let registry = &shared.natives.native_methods;
        for (i, (method, desc)) in dh::UNSAFE_QU_EXTRA_GATE_BITS.iter().enumerate() {
            let id =
                shared.jit.unsafe_inline.qu_extra_sites[i].resolve(registry, CLASS, method, desc);
            let kind = id.and_then(|id| registry.kind_of_id(id));
            let refused = direct_helper_refusal(
                shared,
                CLASS,
                method,
                desc,
                Some((
                    cratonvm_native_api::CapabilityKind::RawMemory,
                    CapabilityQuestion::Call(id),
                )),
                kind,
            );
            if !refused {
                bits |= 1 << (dh::UNSAFE_QU_EXTRA_FIRST_BIT + i);
            }
        }
    }
    bits | unsafe_ref_array_admitted_bits(shared)
}

/// j27 wave 6, lane T1DISP20 (DISP20-8, `CRATONVM_JIT_UNSAFE_REF_ARRAY_INLINE`,
/// default ON since 2026-10-28, j27 WARNING flip; =0 restores): admission bits 34 / 35, the reference-array element arm of the
/// `compareAndSetReference` / reference volatile get prefixes. Set only where
/// the natives' element CAS is the one hardware `LOCK CMPXCHG` on the 8-byte
/// slot (`vm_exec::hw_cas_reference_array_element`: Generational or G1, no
/// compressed oops, `CRATONVM_UNSAFE_REF_ARRAY_HW_CAS` on) and the row's thin
/// helper gate would not refuse. Compile-time only.
pub fn unsafe_ref_array_admitted_bits(shared: &crate::vm::SharedVm) -> usize {
    let heap = &shared.mem.heap;
    let gen_or_g1 = heap.is_generational() || heap.is_g1();
    let unarmed_zgc = crate::vm::heap_is_zgc(shared) && !cratonvm_types::zgc_read_barrier_armed();
    let narrow_oops = cratonvm_types::narrow_oop::narrow_oops_enabled();
    let hw_cas = crate::runtime::env_cache::unsafe_ref_array_hw_cas_enabled();
    let mut bits = 0usize;
    use cratonvm_jit::unsafe_ref_array_arm as unsafe_ra;
    if unsafe_ra::unsafe_ref_array_heap_admits(gen_or_g1, narrow_oops, hw_cas)
        && !direct_helper_tail_would_refuse(shared, &UNSAFE_CAS_REFERENCE_GATE)
    {
        bits |= 1 << unsafe_ra::UNSAFE_REF_ARRAY_CAS_GATE_BIT;
    }
    if unsafe_ra::unsafe_ref_array_heap_admits(gen_or_g1 || unarmed_zgc, narrow_oops, hw_cas)
        && !direct_helper_tail_would_refuse(shared, &UNSAFE_GET_REFERENCE_VOLATILE_GATE)
    {
        bits |= 1 << unsafe_ra::UNSAFE_REF_ARRAY_GET_GATE_BIT;
    }
    bits
}

pub fn unsafe_inline_admitted_mask(shared: &crate::vm::SharedVm) -> usize {
    if let Some(caps) = shared.natives.native_methods.capabilities() {
        if caps.mode() != cratonvm_native_api::CapabilityMode::Permissive {
            return 0;
        }
        // The record a served call would have made (see above). `Permissive`
        // never refuses; an `Err` here would mean the mode moved under us,
        // and then nothing is admitted.
        if crate::vm::vm_exec::check_native_dispatch_capability_for_slot(
            shared,
            cratonvm_native_api::CapabilityKind::RawMemory,
            None,
        )
        .is_err()
        {
            return 0;
        }
    }
    // In `UnsafeInlineAccess::GATE_BITS` order; the test below pins the two
    // lists to each other.
    // nio21 wave 4 L4: bits 10..14 are the volatile int/long rows; nio21
    // wave 5 L3: bits 14 and 15 the CAS rows the JIT's RMW prefixes ask.
    let gates: [&'static DirectHelperGate; 16] = [
        &UNSAFE_GET_BYTE_GATE,
        &UNSAFE_GET_SHORT_GATE,
        &UNSAFE_GET_CHAR_GATE,
        &UNSAFE_GET_INT_GATE,
        &UNSAFE_GET_LONG_GATE,
        &UNSAFE_PUT_BYTE_GATE,
        &UNSAFE_PUT_SHORT_GATE,
        &UNSAFE_PUT_CHAR_GATE,
        &UNSAFE_PUT_INT_GATE,
        &UNSAFE_PUT_LONG_GATE,
        &UNSAFE_GET_INT_VOLATILE_GATE,
        &UNSAFE_GET_LONG_VOLATILE_GATE,
        &UNSAFE_PUT_INT_VOLATILE_GATE,
        &UNSAFE_PUT_LONG_VOLATILE_GATE,
        &UNSAFE_CAS_INT_GATE,
        &UNSAFE_CAS_LONG_GATE,
    ];
    let mut mask = 0usize;
    for (bit, gate) in gates.into_iter().enumerate() {
        if !direct_helper_tail_would_refuse(shared, gate) {
            mask |= 1 << bit;
        }
    }
    // A compiled `LOCK CMPXCHG` / `LOCK XADD` on an array element is only in
    // the natives' atomicity domain while their element CAS is a hardware one
    // (`vm_exec::hw_cas_primitive_array_element`); under its kill switch the
    // CAS rows (and so every RMW prefix) stay on their calls.
    if !crate::runtime::env_cache::unsafe_array_hw_cas_enabled() {
        mask &= !(0b11 << 14);
    }
    mask
}


/// [`direct_helper_table`] for a compile in `shared`, with the compile-time
/// static-slot resolver.
///
/// The resolver is never called from generated code, only by the backends
/// while emitting, so it needs no `JitRuntimeHelpers` ABI slot. Its answer is
/// per VM (`ClassId`s are), so the `SharedVm` pointer travels with it on the
/// compile. It used to be a process-wide registration latched to the first VM,
/// which a second VM had to poison for both.
/// nio20 wave 4, lane L3 (page
/// `r20-sp-thread-current-thread-prefix-needs-a-per-vm-thread-word-FIXED-20261012.md`):
/// `cratonvm_jit::DirectHelperTable::thread_java_obj_offset` for `shared` --
/// `offset_of!(JvmThread, java_thread_obj)` when [`jit_thread_current_thread_direct`]'s
/// gate can never refuse for this VM's life, else `0` (no prefix).
///
/// The gate ([`jit_direct_helper_refused`]) refuses only (a) under
/// `--real-jdk`, per call, where the strict policy may decline the triple
/// (since nio21 wave 4 L4 the word is published there too when
/// [`current_thread_word_admitted_jdk_only`] proves it cannot), and (b) in any
/// mode, a capability-classified gate.
/// `THREAD_CURRENT_THREAD_GATE` is not classified, and the execution policy
/// is the VM's config (`vm_exec::dispatch_policy`), fixed at creation, so
/// outside `--real-jdk` the helper's fast arm serves every call that finds a
/// thread with a mirror -- which is exactly what the prefix reads, and it
/// declines (to this same helper) for no thread or no mirror. The prefix
/// makes no Rust crossing, so it owes no `note_jit_boundary`, and the
/// helper's `native_pending_return` set/clear pair has no net effect (both
/// happen inside one call). `CRATONVM_JIT_SP_CURRENT_THREAD_PREFIX=0` keeps the
/// word `0`.
pub fn thread_java_obj_offset_for(shared: &crate::vm::SharedVm) -> usize {
    // `Option<ObjectRef>` is one pointer and `None` is the null word (the
    // `NonNull` niche, `types/src/value.rs`), so the prefix's `TEST; JZ`
    // reads `None` exactly.
    const _: () = assert!(
        std::mem::size_of::<Option<cratonvm_types::ObjectRef>>() == std::mem::size_of::<usize>()
    );
    if !cfg!(target_arch = "x86_64")
        || THREAD_CURRENT_THREAD_GATE.capability.is_some()
        || THREAD_CURRENT_CARRIER_THREAD_GATE.capability.is_some()
    {
        return 0;
    }
    if crate::vm::dispatch_policy(shared).is_jdk_only()
        && !current_thread_word_admitted_jdk_only(shared)
    {
        return 0;
    }
    std::mem::offset_of!(JvmThread, java_thread_obj)
}

/// nio21 wave 4 L4 (pages `r21-jdk-only-current-thread-prefix-is-never-published-FIXED-20261008.md`
/// and `r21-current-thread-jdk-only-needs-a-classified-gate-FIXED-20261008.md`): may
/// the thread word be published under `--real-jdk`?
///
/// The word lets both tiers answer `Thread.currentThread()` AND
/// `Thread.currentCarrierThread()` (the same prefix serves both binds) with no
/// call, i.e. with no per-call gate. Under `--real-jdk` the gate
/// ([`direct_helper_gate_refuses`] -> [`direct_helper_refusal`]) can refuse for
/// three reasons, and this admits only when none of them can arise for the
/// life of the code compiled against the table:
///
/// * `CRATONVM_ENFORCE_NATIVE_SHADOW` -- a memoised process constant, asked
///   here;
/// * the triple's registered kind -- re-read per call because a
///   re-registration rewrites the slot in place. Since nio21 wave 3 L1 the boot
///   registrar states `currentThread` an `Intrinsic` (HotSpot's
///   `_currentThread`, a JDK `native` with no competing bytecode); requiring
///   `Intrinsic` here is what makes the admission a POLICY fact rather than an
///   absence-of-bytecode one. The residual (a JNI `RegisterNatives` on
///   `java/lang/Thread` demoting it later) is the one wave 3 L1 accepted;
/// * the per-call answer itself -- asked once now for BOTH triples through the
///   same [`direct_helper_tail_would_refuse`] the helpers ask, so a carrier
///   triple the registry does not know (or a stub) keeps the word `0` for both.
///
/// `CRATONVM_JIT_THREAD_WORD_ALL_MODES=0` keeps the pre-wave-4 answer
/// (`0` under `--real-jdk`). Compile-time only.
pub fn current_thread_word_admitted_jdk_only(shared: &crate::vm::SharedVm) -> bool {
    if crate::runtime::env_cache::jdk_only_enforce_shadow()
    {
        return false;
    }
    let (class, method, descriptor) = THREAD_CURRENT_THREAD_GATE.triple();
    let registry = &shared.natives.native_methods;
    let kind = registry
        .resolve_id(class, method, descriptor)
        .and_then(|id| registry.kind_of_id(id));
    kind == Some(cratonvm_native_api::NativeKind::Intrinsic)
        && !direct_helper_tail_would_refuse(shared, &THREAD_CURRENT_THREAD_GATE)
        && !direct_helper_tail_would_refuse(shared, &THREAD_CURRENT_CARRIER_THREAD_GATE)
}

pub fn direct_helper_table_for(
    shared: &crate::vm::SharedVm,
) -> cratonvm_jit::DirectHelperTable {
    let mut table = cratonvm_jit::DirectHelperTable {
        static_base_resolver: jit_resolve_static_base as *const () as usize,
        // Cast: the `SharedVm` address `jit_resolve_static_base` receives back.
        static_base_resolver_ctx: shared as *const crate::vm::SharedVm as usize,
        class_initialized: jit_class_initialized as *const () as usize,
        well_known_class_id: jit_well_known_class_id as *const () as usize,
        barrier_set: shared.mem.heap.barrier_set(),
        ..direct_helper_table()
    };
    // gcg g4/oo hunk 5.2 (b), applied by g4/jm: under the owed-cycle switch,
    // every byte-form poll tests this VM's inline-start latch; when a low
    // poll page fits in `disp32`, page-form polls handle owed cycles via
    // `GcBarrier::sync_owed_poll_page` instead of a second flag test.
    // x86-64 only: aarch64 has no second test; only generational sets the latch.
    if cfg!(target_arch = "x86_64")
        && shared.mem.heap.is_generational()
        && crate::runtime::interpreter::gc_and_alloc::gen_conc_owed_poll_enabled()
        && !cratonvm_jit::poll_page::base_fits_disp32(shared.mem.gc_barrier.page_poll_base())
    {
        table.gen_owed_poll_flag_addr = shared.mem.concurrent_gc_state.inline_start_flag_addr();
    }
    // t1 wave 8, lane SAFEPOLL (`CRATONVM_JIT_PAGE_POLL`, default ON on Linux and Windows x86-64 since j27): this VM's
    // poll page pair, mapped by its barrier at construction (`0` with the
    // switch off or no low mapping). Non-zero, the single-pass poll touches
    // the pair instead of testing the flag byte (`emit_page_form_poll`).
    if cfg!(target_arch = "x86_64") {
        table.page_poll_base = shared.mem.gc_barrier.page_poll_base();
    }
    // j27 wave 4 (lane CLONEIR): the C JVMTI event word, which the inline
    // array `clone()` tests for `VMObjectAlloc` / `SampledObjectAlloc` per
    // clone (the thin helper's `clone_vm_object_alloc_armed`). Only this build
    // can arm either event; without it the word stays `0` (no test).
    #[cfg(feature = "experimental-debug")]
    {
        const _: () = assert!(
            crate::debug::NativeEvent::VmObjectAlloc as u32
                | crate::debug::NativeEvent::SampledAlloc as u32
                == cratonvm_jit::direct_helpers::JVMTI_ALLOC_EVENT_BITS,
            "the inline clone's JVMTI mask must name exactly the two allocation events"
        );
        table.jvmti_alloc_events_word = shared.debug.debugger_gates.native_events_addr();
    }
    // nio20 wave 1, lane L3 (NIO19-5): the compiled-`getClass()` words, for
    // the optimizing tier's inline `getClass()` prefix. Allocated here, on the
    // first compile that could bake the address; the thin helper and the leaf
    // arm publish into it from then on (`stamped_class_mirror_publishing_word`).
    // x86-64 only: no other backend reads it.
    if cfg!(target_arch = "x86_64") && cratonvm_jit::getclass_inline_enabled() {
        table.getclass_mirror_words = shared.classes.getclass_mirror_words.base_addr();
    }
    if cfg!(target_arch = "x86_64") && !cratonvm_types::narrow_oop::narrow_oops_enabled() {
        table.zgc_read_barrier_armed_addr = if crate::vm::heap_is_zgc(shared) {
            cratonvm_types::zgc_read_barrier_armed_ptr() as usize
        } else {
            0
        };
        table.zgc_referent_gate_state_addr = shared.mem.heap.zgc_referent_gate_state_ptr();
        table.reference_get0_inline_admitted =
            !direct_helper_tail_would_refuse(shared, &REFERENCE_GET0_GATE);
        table.mirror_fast_table_ptr = shared.classes.mirror_fast_table.base_addr();
        table.mirror_fast_epoch_addr = shared.classes.mirror_fast_table.epoch_addr();
        table.class_get_superclass_inline_admitted =
            !compat_row_refuses_collection_bind(shared, "java/lang/Class", "getSuperclass", "()Ljava/lang/Class;");
        table.class_get_name_inline_admitted =
            !compat_row_refuses_collection_bind(shared, "java/lang/Class", "getName", "()Ljava/lang/String;");
    }
    if cfg!(target_arch = "x86_64")
        && cratonvm_native_builtins::lang_invoke::jit_reflect_accessor_link_enabled()
        && !super::reflect_link_declines_for_hidden_frames(shared)
    {
        let mut dmha_id = shared
            .jit
            .reflect_link_ids
            .accessors[0]
            .load(std::sync::atomic::Ordering::Relaxed);
        if dmha_id == 0 {
            if let Some(cid) = shared
                .classes
                .class_manager
                .read()
                .find_unique_class_by_name(super::REFLECT_ACCESSOR_CLASS)
            {
                dmha_id = cid.as_u32();
                shared.jit.reflect_link_ids.accessors[0]
                    .store(dmha_id, std::sync::atomic::Ordering::Relaxed);
            }
        }
        if dmha_id != 0 {
            table.dmha_class_id = dmha_id;
            table.reflect_invoke_cache =
                &*shared.jit.reflect_link_ids.invoke_cache as *const cratonvm_jit::ReflectInvokeCache
                    as usize;
            shared.jit.jit_cache.set_reflect_epoch(std::sync::Arc::clone(&shared.jit.reflect_link_ids.gc_epoch));
        table.reflect_gc_epoch_addr =
                &*shared.jit.reflect_link_ids.gc_epoch as *const std::sync::atomic::AtomicU64 as usize;
            table.reflect_hidden_observed_addr =
                shared.natives.native_methods.hidden_frames_observed_addr();
            table.reflect_boolean_true_addr =
                &shared.jit.reflect_link_ids.boolean_true as *const std::sync::atomic::AtomicU64
                    as usize;
            table.reflect_boolean_false_addr =
                &shared.jit.reflect_link_ids.boolean_false as *const std::sync::atomic::AtomicU64
                    as usize;
            table.reflect_method_invoke_cache =
                &*shared.jit.reflect_link_ids.method_invoke_cache
                    as *const cratonvm_jit::ReflectInvokeCache as usize;
        }
    }
    // nio20 wave 1, lane L2 (NIO19-2): which `Unsafe` primitive accessors the
    // optimizing tier may answer inline for a heap primitive array.
    table.unsafe_inline_admitted = unsafe_inline_admitted_mask(shared);
    // nio21 wave 7 L3 (W6-L3-2): the compareAndExchange rows (bits 16, 17),
    // asked only when the capability half admitted anything at all.
    if table.unsafe_inline_admitted != 0 {
        table.unsafe_inline_admitted |= unsafe_cae_admitted_bits(shared);
        // JIT round 20 wave 12, lane qUnsafe: the reference CAS row (bit 18)
        // and the boolean / float / double rows (bits 19..25).
        table.unsafe_inline_admitted |= unsafe_qu_admitted_bits(shared);
        // JIT round 21, lane atomics (UNSAFE21-2): the sub-word CAS rows
        // (bits 29..33).
        table.unsafe_inline_admitted |= unsafe_subword_admitted_bits(shared);
    }
    // nio21 wave 7 L3 (W6-L3-1): the instance-field arm's cell table.
    // JIT round 20 wave 8c (lane huntDisp): not under
    // `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS` -- the arm keys cells by the raw
    // `Unsafe` offset as a slot index (`slot << 1 | wide`), which a byte field
    // offset is not; without the table no field arm is emitted and every field
    // RMW takes its call, whose natives decode the offset.
    // JIT round 21 wave 1, lane updaters
    // (`CRATONVM_JIT_UNSAFE_FIELD_CELLS_BYTE_OFFSETS`, opt-in): a byte-offset
    // VM bakes the table TAGGED, and the screen decodes `16 + slot * 8` to the
    // slot the cells are keyed by (`cratonvm_jit::direct_helpers::
    // UNSAFE_FIELD_CELLS_BYTE_OFFSETS_TAG`). Off, nothing changes: no table and
    // the publisher stays unarmed.
    table.unsafe_field_cells = if shared.classes.unsafe_byte_field_offsets {
        if cratonvm_jit::direct_helpers::unsafe_field_cells_byte_offsets_enabled() {
            match shared.jit.unsafe_inline.base_addr_for_compile() {
                0 => 0,
                base => base | cratonvm_jit::direct_helpers::UNSAFE_FIELD_CELLS_BYTE_OFFSETS_TAG,
            }
        } else {
            0
        }
    } else {
        shared.jit.unsafe_inline.base_addr_for_compile()
    };
    // JIT round 20 wave 12, lane qUnsafe (AT-4): a compile that bakes the
    // table AND admits the reference CAS row arms the reference half's
    // publisher (`hw_atomic_addr`); off (`=0`), no reference cell is ever
    // published.
    // JIT round 21 wave 1, lane updaters: or the reference volatile get /
    // put rows (UPDATERS21-1), which read the same reference half.
    if table.unsafe_field_cells != 0
        && table.unsafe_inline_admitted
            & ((1 << cratonvm_jit::direct_helpers::UNSAFE_REF_CAS_GATE_BIT)
                | (1 << cratonvm_jit::direct_helpers::UNSAFE_REF_VOLATILE_GET_GATE_BIT)
                | (1 << cratonvm_jit::direct_helpers::UNSAFE_REF_VOLATILE_PUT_GATE_BIT)
                // JIT round 23 wave 1, lane L9: the getAndSet field arm.
                | (1 << cratonvm_jit::direct_helpers::UNSAFE_REF_GET_AND_SET_GATE_BIT))
            != 0
    {
        shared.jit.unsafe_inline.arm_ref_cells();
    }
    // nio20 wave 4, lane L3: the `Thread.currentThread()` single-pass prefix's
    // thread word.
    table.thread_java_obj_offset = thread_java_obj_offset_for(shared);
    if !crate::vm::dispatch_policy(shared).is_jdk_only() {
        table.thread_local_get = if super::fast_native::thread_local_compat_fast_serves(shared) {
            super::fast_native::jit_compat_thread_local_get_direct as *const () as usize
        } else {
            0
        };
    }
    table.vthread_state_offset = (core::mem::offset_of!(JvmThread, jdk_continuation) + 4) as u32;
    if shared.config.vthread_compiled_learned && cratonvm_jit::deopt_real_enabled() {
        table.vthread_state_mask = (crate::threading::jvm_thread::JdkContinuationState::JDK >> 32) as u32;
    }
    // Round 15 wave 2 (lane vh2): no thin `VarHandle` bind once this VM has an
    // exact handle to judge; `0` is every door's "not wired". Round 15 wave 6
    // (lane vh8): unless the ladder binds them JUDGED, which it then does.
    if !varhandle_method_entry_binds_admitted(shared) {
        table.varhandle_read = [0; cratonvm_jit::VARHANDLE_READ_SLOTS];
        table.varhandle_write = [0; cratonvm_jit::VARHANDLE_WRITE_SLOTS];
        table.varhandle_cas = [0; cratonvm_jit::VARHANDLE_CAS_SLOTS];
    }
    // Round 15 wave 2 hand-back (lane vh2; VH2-3): no read slot whose site
    // needs a reference cast the helper cannot make.
    for (slot, cell) in table.varhandle_read.iter_mut().enumerate() {
        if !varhandle_read_slot_bindable(slot) {
            *cell = 0;
        }
    }
    table
}

































// ---------------------------------------------------------------------------
// nio21 wave 19, lane L3: the `ThreadLocal.get()` expansion's layout
// (`r21-threadlocal-get-has-no-ir-intrinsic-FIXED-20261010.md`).
// ---------------------------------------------------------------------------

/// CompilerOnly (`DirectHelperTable::thread_local_ir_layout`): fill `out`
/// (`n` words, at least [`cratonvm_jit::direct_helpers::THREAD_LOCAL_IR_LAYOUT_WORDS`])
/// with what the optimizing tier's `ThreadLocal.get()` expansion bakes, and
/// answer the word count, or `0` when this VM must keep the call:
///
/// | word | meaning |
/// |---|---|
/// | 0 | byte offset of `JvmThread::java_thread_obj` (the `JIT_THREAD` record) |
/// | 1 | class id of `java/lang/ThreadLocal` |
/// | 2 | class id of `ThreadLocal$SuppliedThreadLocal`, `0` when not loaded or it reads another map |
/// | 3 | class id of `ThreadLocal$ThreadLocalMap` |
/// | 4 | class id of `ThreadLocal$ThreadLocalMap$Entry` |
/// | 5 | field index of `Thread.threadLocals` |
/// | 6 | field index of `ThreadLocalMap.table` |
/// | 7 | field index of `ThreadLocal.threadLocalHashCode` |
/// | 8 | field index of `Reference.referent` (`REF_FIELD_REFERENT`, `0`) |
/// | 9 | field index of `Entry.value` |
/// | 10..=13 | nio21 wave 19b: `1 + compact displacement` of `Thread.threadLocals`, `ThreadLocalMap.table`, `ThreadLocal.threadLocalHashCode`, `Entry.value` in their declaring classes' registered compact layouts (`0` = none, or a storage kind other than the field's) |
/// | 14 | `1` when `ThreadLocal.set` may be expanded too (`jdk.traceVirtualThreadLocals` off: the JDK `set` then runs no tracing code), else `0` |
/// | 15 | JIT round 21 (lane tl2): class id of `java/lang/InheritableThreadLocal` (`0` = its sites keep the call) |
/// | 16 | field index of `Thread.inheritableThreadLocals` |
/// | 17 | `1 + compact displacement` of `Thread.inheritableThreadLocals` (`0` = none) |
/// | 18 | `1 + compact displacement` of `Reference.referent` (slot 0) in `Entry`'s registered compact layout (`0` = none) |
///
/// Served exactly where the thin helper serves (`thread_local_real_fast_serves`:
/// `--real-jdk` with the real-field `ThreadLocal` natives), with the class ids
/// and slots the helper itself reads (`tl_get_layout`), so the expansion and the
/// helper cannot disagree about the table they read. Called by the compiler at
/// compile time, never from generated code.
///
/// # Safety
///
/// `vm_ptr` is the `SharedVm` published beside this function
/// (`static_base_resolver_ctx`), and `out` addresses `n` writable words.
pub unsafe extern "C" fn jit_thread_local_ir_layout(vm_ptr: i64, out: i64, n: i64) -> i64 {
    // JIT round 21 (lane refs2, `CRATONVM_JIT_IR_REFERS_TO_OWN_LAYOUT`): the
    // referent-only query (`n == REFERENCE_REFERENT_QUERY_WORDS`), which the
    // `Reference.refersTo` expansion asks so it no longer waits for the
    // `ThreadLocal` classes the full layout needs (`tl_get_layout` answers
    // `None` until `InheritableThreadLocal` and `TerminatingThreadLocal` are
    // loaded, which a program that never runs `ThreadLocal.set` /
    // `setInitialValue` may never do). Word 0 is `Reference.referent`'s index
    // (`REF_FIELD_REFERENT`, `0`: the full layout's word 8), answered under
    // `--real-jdk` only, the mode gate of the full layout's
    // `thread_local_real_fast_serves` without its `ThreadLocal`-native half,
    // which the referent does not depend on.
    if vm_ptr != 0
        && out != 0
        && n == cratonvm_jit::direct_helpers::REFERENCE_REFERENT_QUERY_WORDS as i64
    {
        let f = || {
            // SAFETY: as the function's contract.
            let vm = &*(vm_ptr as *const SharedVm);
            // t1 wave 12 (lane REFS4): `--compatible` over real class bytes
            // too (`CRATONVM_JIT_IR_REFERS_TO_COMPATIBLE`).
            if !refers_to_referent_query_serves(vm) {
                return 0;
            }
            // SAFETY: `out` addresses `n == 1` writable word.
            (out as *mut u64).write(0);
            1
        };
        return contain("jit_thread_local_ir_layout", OnPanic::RecordFor { vm_ptr }, 0, f);
    }
    // t1 wave 12 (lane REFS4, `CRATONVM_JIT_IR_REFERS_TO_COMPACT_ROW`): the
    // referent-row query (`n == REFERENCE_REFERENT_ROW_QUERY_WORDS`). Word 0
    // is the index (as above), word 1 `1 + compact displacement` of
    // `Reference.referent` in `java/lang/ref/Reference`'s registered compact
    // layout, word 2 `Reference`'s class id. Served where the index query is.
    // The row is named only on Gen and G1: under ZGC the checked helper's
    // referent read runs the resurrection gate (`gate_referent_read`), which
    // an inline read would skip, so a ZGC compile keeps the helper.
    // j27 wave 5 (lane REFS): word 3 is the compare-only row
    // ([`reference_referent_row_words`]).
    if vm_ptr != 0
        && out != 0
        && n == cratonvm_jit::direct_helpers::REFERENCE_REFERENT_ROW_QUERY_WORDS as i64
    {
        let f = || {
            // SAFETY: as the function's contract.
            let vm = &*(vm_ptr as *const SharedVm);
            if !refers_to_referent_query_serves(vm) {
                return 0;
            }
            let words = reference_referent_row_words(vm);
            let dst = out as *mut u64;
            for (i, w) in words.iter().enumerate() {
                // SAFETY: `out` addresses `n == 4` writable words.
                dst.add(i).write(*w);
            }
            words.len() as i64
        };
        return contain("jit_thread_local_ir_layout", OnPanic::RecordFor { vm_ptr }, 0, f);
    }
    // j27 wave 5 (lane REFS): the referent-gate query
    // (`n == REFERENT_GATE_MAY_ARM_QUERY_WORDS`): word 0 is `1` when this
    // heap may ever arm the ZGC referent read gate
    // (`VmHeap::zgc_referent_gate_may_arm`), which only the checked helper
    // runs, so the lowering keeps every referent read on it. Every mode:
    // the answer is the heap's, not the class library's.
    if vm_ptr != 0
        && out != 0
        && n == cratonvm_jit::direct_helpers::REFERENT_GATE_MAY_ARM_QUERY_WORDS as i64
    {
        let f = || {
            // SAFETY: as the function's contract.
            let vm = &*(vm_ptr as *const SharedVm);
            // SAFETY: `out` addresses `n == 5` writable words.
            (out as *mut u64).write(u64::from(vm.mem.heap.zgc_referent_gate_may_arm()));
            1
        };
        return contain("jit_thread_local_ir_layout", OnPanic::RecordFor { vm_ptr }, 0, f);
    }
    if vm_ptr != 0
        && out != 0
        && n == cratonvm_jit::direct_helpers::NIO_CHARSET_IR_LAYOUT_QUERY_WORDS as i64
    {
        let f = || {
            // SAFETY: as the function's contract.
            let vm = &*(vm_ptr as *const SharedVm);
            let words = super::fast_native::nio_charset_ir_layout_words(vm);
            let Some(words) = words else {
                return 0;
            };
            let dst = out as *mut u64;
            for (i, w) in words.iter().enumerate() {
                // SAFETY: `out` addresses `NIO_CHARSET_IR_LAYOUT_QUERY_WORDS` writable words.
                dst.add(i).write(*w);
            }
            words.len() as i64
        };
        return contain("jit_thread_local_ir_layout", OnPanic::RecordFor { vm_ptr }, 0, f);
    }
    // JIT round 21 (lane g1vhsoft, `CRATONVM_JIT_IR_REFERENCE_GET_INLINE`):
    // the `WeakReference.get()` / `SoftReference.get()` expansion's words
    // (`n == REFERENCE_GET_LAYOUT_QUERY_WORDS`; their meaning is
    // `cratonvm_jit::ir::ReferenceGetIrLayout`'s table), from
    // [`reference_get_ir_layout_words`].
    if vm_ptr != 0
        && out != 0
        && n == cratonvm_jit::direct_helpers::REFERENCE_GET_LAYOUT_QUERY_WORDS as i64
    {
        let f = || {
            // SAFETY: as the function's contract.
            let vm = &*(vm_ptr as *const SharedVm);
            let Some(words) = reference_get_ir_layout_words(vm) else {
                return 0;
            };
            let dst = out as *mut u64;
            for (i, w) in words.iter().enumerate() {
                // SAFETY: `out` addresses `n == words.len()` writable words.
                dst.add(i).write(*w);
            }
            words.len() as i64
        };
        return contain("jit_thread_local_ir_layout", OnPanic::Record, 0, f);
    }
    // JIT round 21 (lane tlsub, TL321-4): the one-class query
    // (`n == THREAD_LOCAL_RECEIVER_QUERY_WORDS`): word 0 holds the receiver's
    // class id on the way in and `tl_get_receiver_kind`'s answer for it on the
    // way out -- the very verdict the thin helper serves that class by -- and
    // word 1 is `1` when no class below `ThreadLocal` on its chain declares
    // its own `set(Object)` either (`tl_class_declares_set`). Served where the
    // full layout is (`thread_local_real_fast_serves` + `tl_get_layout`).
    if vm_ptr != 0
        && out != 0
        && n == cratonvm_jit::direct_helpers::THREAD_LOCAL_RECEIVER_QUERY_WORDS as i64
    {
        let f = || {
            // SAFETY: as the function's contract.
            let vm = &*(vm_ptr as *const SharedVm);
            if !thread_local_real_fast_serves(vm) {
                return 0;
            }
            // `CRATONVM_JIT_IR_THREAD_LOCAL_EARLY_LAYOUT`: the compiler's
            // layout, which need not wait for `InheritableThreadLocal` /
            // `TerminatingThreadLocal` (`tl_ir_layout`).
            let Some((l, complete)) = tl_ir_layout(vm) else {
                return 0;
            };
            let io = out as *mut u64;
            // SAFETY: `out` addresses `n == 2` words; word 0 is the question.
            let Ok(raw) = u32::try_from(io.read()) else {
                return 0;
            };
            if raw == 0 {
                return 0;
            }
            let cid = ClassId::new(raw);
            let kind = tl_ir_receiver_kind(vm, &l, complete, cid);
            let set_ok = kind != TL_MAP_BYTECODE && !tl_class_declares_set(vm, &l, cid);
            // SAFETY: as above, two writable words.
            io.write(u64::from(kind));
            io.add(1).write(u64::from(set_ok));
            2
        };
        return contain("jit_thread_local_ir_layout", OnPanic::Record, 0, f);
    }
    // TLGET21-7 (`CRATONVM_JIT_IR_THREAD_LOCAL_PENDING_TRAP`): the pending
    // question (`n == THREAD_LOCAL_LAYOUT_PENDING_QUERY_WORDS`): word 0 is `1`
    // when this VM serves the expansion but `tl_ir_layout` cannot resolve it
    // yet (a class it bakes is not loaded), so a never-run site may trap
    // instead of keeping its call for the life of its code.
    if vm_ptr != 0
        && out != 0
        && n == cratonvm_jit::direct_helpers::THREAD_LOCAL_LAYOUT_PENDING_QUERY_WORDS as i64
    {
        let f = || {
            // SAFETY: as the function's contract.
            let vm = &*(vm_ptr as *const SharedVm);
            let pending = thread_local_real_fast_serves(vm) && tl_ir_layout(vm).is_none();
            // SAFETY: `out` addresses `n == 3` writable words.
            (out as *mut u64).write(u64::from(pending));
            1
        };
        return contain("jit_thread_local_ir_layout", OnPanic::Record, 0, f);
    }
    if vm_ptr == 0
        || out == 0
        || n < cratonvm_jit::direct_helpers::THREAD_LOCAL_IR_LAYOUT_WORDS as i64
    {
        return 0;
    }
    let f = || {
        // SAFETY: as the function's contract.
        let vm = &*(vm_ptr as *const SharedVm);
        if !thread_local_real_fast_serves(vm) {
            return 0;
        }
        // `CRATONVM_JIT_IR_THREAD_LOCAL_EARLY_LAYOUT` (`tl_ir_layout`): an
        // unloaded `InheritableThreadLocal` is word 15 = 0 (its sites keep
        // the call).
        let Some((l, complete)) = tl_ir_layout(vm) else {
            return 0;
        };
        // tl21: word 8 below bakes `Reference.referent` as field index 0, as
        // the helper's `tl_find_entry` reads slot 0 of an `Entry`. Say so only
        // for a real `Reference` that declares `referent` as its first
        // instance field -- the check the referent-only queries above already
        // make -- so a class library that reordered it keeps the call.
        if reference_class_with_referent_first(vm).is_none() {
            return 0;
        }
        let (map, supplied, thread) = {
            let cm = vm.classes.class_manager.read();
            (
                cm.get_loaded_class_id("java/lang/ThreadLocal$ThreadLocalMap"),
                cm.get_loaded_class_id("java/lang/ThreadLocal$SuppliedThreadLocal"),
                cm.get_loaded_class_id("java/lang/Thread"),
            )
        };
        let (Some(map), Some(thread)) = (map, thread) else {
            return 0;
        };
        // The compact rows the expansion's loads may read inline: the same
        // `compact_field_disp_storage(declaring class, index)` answer the
        // interpreter's eager compile and the compile door's field resolver
        // hand the lowering for an ordinary `getfield` of these fields.
        let row = |decl: cratonvm_types::ClassId, index: usize, want_ref: bool| -> u64 {
            match cratonvm_types::compact_field_disp_storage(decl.as_u32(), index) {
                Some((off, storage))
                    if (want_ref && storage.is_reference())
                        || (!want_ref
                            && matches!(storage, cratonvm_types::FieldStorageKind::Int)) =>
                {
                    off as u64 + 1
                }
                _ => 0,
            }
        };
        // `withInitial`'s class answers only when it reads the plain map
        // (it overrides `initialValue`, never `get`).
        let supplied = supplied
            .filter(|&s| tl_ir_receiver_kind(vm, &l, complete, s) == TL_MAP_PLAIN)
            .map_or(0, |s| u64::from(s.as_u32()));
        let words: [u64; cratonvm_jit::direct_helpers::THREAD_LOCAL_IR_LAYOUT_WORDS] = [
            std::mem::offset_of!(JvmThread, java_thread_obj) as u64,
            u64::from(l.tl.as_u32()),
            supplied,
            u64::from(map.as_u32()),
            u64::from(l.entry.as_u32()),
            l.thread_tl_slot as u64,
            l.table_slot as u64,
            l.hash_slot as u64,
            0,
            l.value_slot as u64,
            row(thread, l.thread_tl_slot, true),
            row(map, l.table_slot, true),
            row(l.tl, l.hash_slot, false),
            row(l.entry, l.value_slot, true),
            u64::from(!l.trace),
            // JIT round 23 wave 2f, lane L4 (W19L3-1,
            // `CRATONVM_JIT_IR_INHERITABLE_THREAD_LOCAL`, default ON, `0`
            // keeps the call): an exact `InheritableThreadLocal` receiver,
            // whose `getMap` is `t.inheritableThreadLocals` (the helper's own
            // `TL_MAP_INHERITABLE` arm reads the same field).
            if l.itl.as_u32() != 0
                && tl_ir_receiver_kind(vm, &l, complete, l.itl) == TL_MAP_INHERITABLE
            {
                u64::from(l.itl.as_u32())
            } else {
                0
            },
            l.thread_itl_slot as u64,
            row(thread, l.thread_itl_slot, true),
            // TL321-5: the referent's row in `Entry`'s own layout (its
            // inherited slot 0), so the expansion's `refersTo(this)` compare
            // reads a compact `Entry` inline. The expansion only compares the
            // word with a receiver it holds; it never hands it out, which is
            // why the referent gate and keep-alive `jit_getfield` adds for
            // every other referent read are not owed there.
            row(l.entry, 0, true),
        ];
        let dst = out as *mut u64;
        for (i, w) in words.iter().enumerate() {
            // SAFETY: `out` addresses at least `n >= words.len()` words.
            dst.add(i).write(*w);
        }
        words.len() as i64
    };
    contain("jit_thread_local_ir_layout", OnPanic::RecordFor { vm_ptr }, 0, f)
}

/// `CRATONVM_JIT_IR_REFERS_TO_COMPATIBLE` (t1 wave 12, lane REFS4), **default
/// ON**, `=0` restores the `--real-jdk`-only answer: the referent queries of
/// [`jit_thread_local_ir_layout`] answer a `--compatible` VM too when it runs
/// real class bytes (not `use_synthetic_jdk`) and its loaded
/// `java/lang/ref/Reference` is no compatibility stub and declares `referent`
/// as its first instance field ([`reference_class_with_referent_first`]).
/// Then the optimizing tier's `refersTo` expansion runs in both modes (host
/// t1 wave 11: `--compatible` soft 1264.9 / phantom 1182.6 ns per 16 calls
/// on the call path vs HotSpot 4.4). Sound for the reason the `--real-jdk`
/// answer is: every `refersTo` body either mode can run -- the JDK's
/// `refersTo` -> `refersToImpl` -> `refersTo0`, or the `JdkReferences`
/// family's `native_ref_refers_to` stand-in when that family is reverted --
/// is the identity compare of slot `REF_FIELD_REFERENT` without a keep-alive.
pub fn refers_to_compatible_enabled() -> bool {
    true
}

/// t1 wave 12 (lane REFS4): may the referent queries of
/// [`jit_thread_local_ir_layout`] answer `vm`? `--real-jdk` always (the
/// t1 wave 9 rule); `--compatible` under [`refers_to_compatible_enabled`].
pub fn refers_to_referent_query_serves(vm: &SharedVm) -> bool {
    if crate::vm::dispatch_policy(vm).is_jdk_only() {
        return true;
    }
    refers_to_compatible_enabled()
        && !vm.config.use_synthetic_jdk
        && reference_class_with_referent_first(vm).is_some()
}

/// t1 wave 12 (lane REFS4): the loaded `java/lang/ref/Reference`, when it is
/// real class bytes (no compatibility stub), extends `Object` with no
/// inherited instance field, and declares `referent` as its first instance
/// field -- field index `0`, `REF_FIELD_REFERENT`. `None` otherwise.
pub fn reference_class_with_referent_first(vm: &SharedVm) -> Option<ClassId> {
    let cm = vm.classes.class_manager.read();
    let cid = cm.get_loaded_class_id("java/lang/ref/Reference")?;
    let class = cm.class_store.get(cid)?;
    if class.origin.is_compatibility_stub() || class.first_field_index != 0 {
        return None;
    }
    let first = class.fields.iter().find(|f| !f.is_static())?;
    (&*first.name == "referent" && first.descriptor.starts_with('L')).then_some(cid)
}

/// The referent-row query's answer
/// (`cratonvm_jit::direct_helpers::REFERENCE_REFERENT_ROW_ANSWER_WORDS`):
/// `[index, handed-out row, Reference class id, compare-only row]`.
///
/// Both rows are `1 + compact displacement` of `Reference.referent` (`0` =
/// none) and differ only in what the read owes besides the slot word:
/// * the handed-out row (`Reference.get`) -- Gen and G1 only: under ZGC the
///   keep-alive is the load barrier's mark step and the resurrection gate
///   (`gate_referent_read`), neither of which the inline read runs;
/// * the compare-only row (j27 wave 5, lane REFS: `refersTo`, which owes no
///   keep-alive) -- on every collector. Where the referent read gate may arm
///   (`VmHeap::zgc_referent_gate_may_arm`), the lowering reads it inline only
///   for a compare against a value that is non-null or tested for null first
///   (`cratonvm_jit::ir_lower::referent_read_owes_the_checked_helper`): the
///   gate answers null only for a dead referent, which equals no value the
///   program holds, so only `refersTo(null)` needs the checked helper.
pub fn reference_referent_row_words(
    vm: &SharedVm,
) -> [u64; cratonvm_jit::direct_helpers::REFERENCE_REFERENT_ROW_ANSWER_WORDS] {
    let reference = reference_class_with_referent_first(vm);
    let row = reference.map_or(0, |cid| {
        match cratonvm_types::compact_field_disp_storage(cid.as_u32(), 0) {
            Some((off, storage)) if storage.is_reference() => off as u64 + 1,
            _ => 0,
        }
    });
    let heap = &vm.mem.heap;
    let handed_out = if heap.is_generational() || heap.is_g1() { row } else { 0 };
    let compare_only = row;
    let class = reference.map_or(0, |c| u64::from(c.as_u32()));
    [0, handed_out, class, compare_only]
}


/// JIT round 21 (lane g1vhsoft, `CRATONVM_JIT_IR_REFERENCE_GET_INLINE`): the
/// words of `cratonvm_jit::ir::ReferenceGetIrLayout` for this VM, or `None`
/// when no `get()` may be expanded.
///
/// A class is answered only where the JDK body is exactly what every door
/// runs for an exactly-that-class receiver: no registered `get()` native on
/// `Reference`, nor on the class itself, would run
/// ([`crate::runtime::interpreter::registered_native_will_run`], asked in
/// both modes: a `--compatible` VM whose `JdkReferences` alignment row keeps
/// a `get` bridge answers nothing for it), and the boot class is a real one
/// (not a compatibility stub) whose fields resolve by name AND descriptor:
/// `Reference.referent` (`Ljava/lang/Object;`), `SoftReference.timestamp`
/// (`J`, instance) and `SoftReference.clock` (`J`, static, its index among
/// the class's own statics, as `get_static_shared` counts them). Anything
/// missing answers `0` for that class (word 0 / word 1), or `None` when
/// neither class is left. The compact rows are the compile door's own
/// `compact_field_disp_storage(declaring class, index)` answer for the EXACT
/// class (a parent's compact layout is a prefix of every child's), `0` when
/// none or of another storage kind. Compile time only.
pub fn reference_get_ir_layout_words(
    vm: &SharedVm,
) -> Option<[u64; cratonvm_jit::direct_helpers::REFERENCE_GET_LAYOUT_QUERY_WORDS]> {
    const GET: &str = "get";
    const GET_DESC: &str = "()Ljava/lang/Object;";
    let will_run = |class: &str| {
        crate::runtime::interpreter::registered_native_will_run(vm, class, GET, GET_DESC)
    };
    if will_run("java/lang/ref/Reference") {
        return None;
    }
    let weak_ok = !will_run("java/lang/ref/WeakReference");
    let soft_ok = !will_run("java/lang/ref/SoftReference");
    if !weak_ok && !soft_ok {
        return None;
    }
    let row = |decl: ClassId, index: usize, want: cratonvm_types::FieldStorageKind| -> u64 {
        match cratonvm_types::compact_field_disp_storage(decl.as_u32(), index) {
            Some((off, storage)) if storage == want => off as u64 + 1,
            _ => 0,
        }
    };
    let cm = vm.classes.class_manager.read();
    let real = |name: &str| {
        cm.find_bootstrap_class_by_name(name).filter(|&id| {
            cm.get_class(id)
                .is_some_and(|c| !c.origin.is_compatibility_stub())
        })
    };
    let reference = real("java/lang/ref/Reference")?;
    let referent = crate::vm::resolve_field_index_in_hierarchy_desc(
        reference,
        "referent",
        Some("Ljava/lang/Object;"),
        &cm.class_store,
    )?;
    let mut words = [0u64; cratonvm_jit::direct_helpers::REFERENCE_GET_LAYOUT_QUERY_WORDS];
    words[2] = referent as u64;
    // j27 wave 5 (lane REFS; GC correctness, no switch): no referent row on a
    // heap whose referent read gate may arm (ZGC with the after-release
    // referent clear). The row's inline read runs the SATB keep-alive gate
    // but not the resurrection gate (`gate_referent_read`), so it could hand
    // out a referent the pause found dead, which the parked sweep then frees
    // while the program holds it. Row-less, a compact receiver takes the
    // checked helper, which runs both.
    let referent_row = |decl: ClassId| -> u64 {
        if vm.mem.heap.zgc_referent_gate_may_arm() {
            return 0;
        }
        row(decl, referent, cratonvm_types::FieldStorageKind::Reference)
    };
    if let Some(weak) = real("java/lang/ref/WeakReference").filter(|_| weak_ok) {
        words[0] = u64::from(weak.as_u32());
        words[3] = referent_row(weak);
    }
    if let Some(soft) = real("java/lang/ref/SoftReference").filter(|_| soft_ok) {
        let timestamp = crate::vm::resolve_field_index_in_hierarchy_desc(
            soft,
            "timestamp",
            Some("J"),
            &cm.class_store,
        );
        // `clock`: its index among `SoftReference`'s OWN static fields.
        let clock = cm.get_class(soft).and_then(|c| {
            c.fields
                .iter()
                .filter(|f| f.is_static())
                .enumerate()
                .find(|(_, f)| &*f.name == "clock" && &*f.descriptor == "J")
                .map(|(i, f)| (i, f.is_volatile()))
        });
        if let (Some(timestamp), Some((clock, clock_volatile))) = (timestamp, clock) {
            words[1] = u64::from(soft.as_u32());
            words[4] = referent_row(soft);
            words[5] = timestamp as u64;
            words[6] = row(soft, timestamp, cratonvm_types::FieldStorageKind::Long);
            words[7] = clock as u64;
            words[8] = u64::from(clock_volatile);
        }
    }
    (words[0] != 0 || words[1] != 0).then_some(words)
}















