
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
    static JIT_SELF_CALL_STACK_FLOOR: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// Query the current thread's native stack bounds `(low, high)` from the OS.
/// `None` when the platform query is unavailable/fails — the caller falls
/// back to a conservative offset from the first observed stack pointer.
#[cfg(windows)]
pub(crate) fn thread_stack_bounds() -> Option<(usize, usize)> {
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
pub(crate) fn thread_stack_bounds() -> Option<(usize, usize)> {
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
pub(crate) fn thread_stack_bounds() -> Option<(usize, usize)> {
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
pub(crate) fn thread_stack_bounds() -> Option<(usize, usize)> {
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
const SELF_CALL_STACK_BUDGET: usize = 4 << 20; // 4 MiB

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
const SELF_CALL_BUDGET_PER_JAVA_STACK_BYTE: usize = 16;

/// HotSpot's default thread stack size on Windows and Linux x64 (1 MiB): the
/// Java stack size of a thread with no `-Xss` and no `stackSize` request.
const HOTSPOT_DEFAULT_JAVA_STACK_BYTES: usize = 1 << 20;

/// The per-thread inputs of the compiled-recursion budget, copied out of the
/// [`JvmThread`] (per VM: the thread's own recorded request, from `VmConfig`'s
/// `-Xss` or its constructor's `stackSize`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SelfCallBudget {
    /// [`JvmThread::self_call_extra_budget`]: the carrier growth (flat mode).
    pub(crate) extra: usize,
    /// [`JvmThread::self_call_java_stack_bytes`]: the thread's Java stack
    /// size, `0` for HotSpot's default.
    pub(crate) java_stack: usize,
}

impl SelfCallBudget {
    /// The budget inputs `thread` recorded.
    pub(crate) fn of_thread(thread: &JvmThread) -> Self {
        Self {
            extra: thread.self_call_extra_budget,
            java_stack: thread.self_call_java_stack_bytes,
        }
    }

    /// The byte budget below the first observed stack pointer. Read on the
    /// cold floor computations only.
    fn bytes(self) -> usize {
        if cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JIT_SELF_CALL_BUDGET_HOTSPOT_DEPTH",
        ) {
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
            && cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JIT_SELF_CALL_BUDGET_FOLLOWS_STACK_SIZE",
            ) {
            self.extra
        } else {
            0
        };
        SELF_CALL_STACK_BUDGET.saturating_add(extra)
    }
}

#[cfg(test)]
mod r23_w6_self_call_budget_tests {
    use super::*;

    const K: usize = 1024;
    const M: usize = 1024 * 1024;

    fn b(extra: usize, java_stack: usize) -> SelfCallBudget {
        SelfCallBudget { extra, java_stack }
    }

    /// 16 x the Java stack size (HotSpot's 1 MiB default when unset), the
    /// carrier growth ignored; the kill switch restores the flat 4 MiB plus
    /// the growth.
    #[test]
    fn r23_w6_the_budget_follows_the_java_stack_size() {
        let key = concat!("CRATON", "VM_JIT_SELF_CALL_BUDGET_HOTSPOT_DEPTH");
        cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
            assert_eq!(b(0, 0).bytes(), 16 * M, "default -Xss");
            assert_eq!(b(0, M).bytes(), 16 * M);
            assert_eq!(b(0, 256 * K).bytes(), 4 * M, "-Xss256k");
            assert_eq!(b(16 * M, 16 * M).bytes(), 256 * M, "-Xss16m");
        });
        cratonvm_types::flags::with_thread_overrides(&[(key, Some("0"))], || {
            assert_eq!(b(0, 0).bytes(), 4 * M);
            assert_eq!(b(0, 256 * K).bytes(), 4 * M, "flat");
            assert_eq!(b(16 * M, 16 * M).bytes(), 20 * M, "flat plus the carrier growth");
        });
    }

    /// The thread's guard floor still binds a budget larger than its stack.
    #[test]
    fn r23_w6_the_guard_floor_still_bounds_the_budget() {
        let key = concat!("CRATON", "VM_JIT_SELF_CALL_BUDGET_HOTSPOT_DEPTH");
        let sp = 0x10_0000_0000usize;
        let roomy = Some((sp - 300 * M, sp + M));
        let tight = Some((sp - 8 * M, sp + M));
        cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
            assert_eq!(compute_self_call_stack_floor_in(sp, b(0, 0), roomy), sp - 16 * M);
            assert_eq!(compute_self_call_stack_floor_in(sp, b(0, 256 * K), roomy), sp - 4 * M);
            assert_eq!(compute_self_call_stack_floor_in(sp, b(0, 16 * M), roomy), sp - 256 * M);
            // A 9 MiB stack: the guard floor (1 MiB above its low end) binds.
            assert_eq!(compute_self_call_stack_floor_in(sp, b(0, 0), tight), sp - 8 * M + M);
        });
        cratonvm_types::flags::with_thread_overrides(&[(key, Some("0"))], || {
            assert_eq!(compute_self_call_stack_floor_in(sp, b(0, 0), roomy), sp - 4 * M);
        });
    }
}

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
fn compute_self_call_stack_floor(sp_now: usize, budget: SelfCallBudget) -> usize {
    compute_self_call_stack_floor_in(sp_now, budget, thread_stack_bounds())
}

/// [`compute_self_call_stack_floor`] for a thread whose stack bounds the
/// caller already holds (`None`: unknown). Interpreter round i3 wave 79, lane
/// L3: the reflective doors pass the bounds they cache, so a rebase asks the
/// OS nothing ([`rebase_self_call_stack_floor`]).
#[cold]
fn compute_self_call_stack_floor_in(
    sp_now: usize,
    budget: SelfCallBudget,
    bounds: Option<(usize, usize)>,
) -> usize {
    let guard_floor = match bounds {
        Some((low, high)) => {
            let size = high - low;
            low.saturating_add(self_call_floor_margin(
                size,
                cratonvm_types::flags::runtime_flag_default_on(
                    "CRATONVM_JIT_SELF_CALL_FLOOR_STRIDE_MARGIN",
                ),
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
fn current_self_call_budget() -> SelfCallBudget {
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
const SELF_CALL_SAMPLE_STRIDE: usize = 64 * 1024;

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
fn self_call_floor_margin(size: usize, stride_margin: bool) -> usize {
    const HEADROOM: usize = 1 << 20; // 1 MiB
    let legacy = HEADROOM.min(size / 4).max(64 * 1024);
    if stride_margin {
        legacy.max((3 * SELF_CALL_SAMPLE_STRIDE).min(size / 2))
    } else {
        legacy
    }
}

#[cfg(test)]
mod r12w3_hfix_self_call_floor_margin_tests {
    use super::*;

    #[test]
    fn a_small_stack_keeps_more_than_one_sample_stride_below_the_floor() {
        const K: usize = 1024;
        // The hunter's case: 256 KiB was one stride; now two (half the stack).
        assert_eq!(self_call_floor_margin(256 * K, false), 64 * K);
        assert_eq!(self_call_floor_margin(256 * K, true), 128 * K);
        // Three strides once the stack can spare them.
        assert_eq!(self_call_floor_margin(512 * K, true), 192 * K);
        assert_eq!(self_call_floor_margin(1024 * K, true), 256 * K);
        // Large stacks keep the full 1 MiB headroom either way.
        for size in [4096 * K, 8192 * K, 128 * 1024 * K] {
            assert_eq!(self_call_floor_margin(size, true), 1024 * K);
            assert_eq!(
                self_call_floor_margin(size, true),
                self_call_floor_margin(size, false)
            );
        }
        // Never more than half of any stack.
        for size in [128 * K, 192 * K, 256 * K, 384 * K] {
            assert!(self_call_floor_margin(size, true) <= (size / 2).max(64 * K));
        }
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
fn current_thread_self_call_floor(budget: SelfCallBudget) -> usize {
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

/// `CRATONVM_JIT_STACK_FLOOR_WORD` (default on): publish
/// `JvmThread::jit_stack_floor`'s displacement to the JIT, so single-pass
/// inline-cache stack checks read the floor through the thread mirror instead
/// of a frame slot filled by a per-entry leaf call. `0` publishes no
/// displacement and every site keeps its frame slot, byte for byte. Read once
/// per helper-table build (one per compile).
fn jit_stack_floor_word_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_STACK_FLOOR_WORD")
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
unsafe fn jit_self_call_stack_guard_body(vm_ptr: i64, ctor_receiver: i64) -> i64 {
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
fn ra_is_reset(ra: usize) -> bool {
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
pub(crate) mod savebase_watcher {
    use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};

    pub static ARM_ADDR: AtomicUsize = AtomicUsize::new(0);
    static WORKER_HANDLE: AtomicIsize = AtomicIsize::new(0);
    static STARTED: AtomicBool = AtomicBool::new(false);

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
unsafe extern "C" fn arm_savebase_watch_inner(addr: i64, ra: usize) {
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

fn ref_store_plan_for_heap(
    heap: Option<&crate::memory::vm_heap::VmHeap>,
) -> ((usize, usize, usize), usize) {
    match heap {
        Some(crate::memory::vm_heap::VmHeap::Generational(heap)) => {
            let (pre, post, floor, mask) = heap.jit_ref_store_plan();
            ((pre, post, floor), usize::from(mask))
        }
        #[cfg(feature = "zgc")]
        Some(crate::memory::vm_heap::VmHeap::Zgc(heap)) => {
            let (pre, post, floor, mask) = heap.jit_ref_store_plan();
            ((pre, post, floor), usize::from(mask))
        }
        // g1put: G1 publishes no PLAN (no post gate, no floor, no mask), so
        // `ref_store_gates_of` still answers `None` and no generational-style
        // gated arm reaches it. But its SATB queue arms the process-wide pre
        // byte, which the optimizing tier's G1 store reads to keep an
        // overwrite of a non-null reference inline while no mark is running
        // (`ir_lower::emit_g1_ir_ref_putfield`).
        Some(crate::memory::vm_heap::VmHeap::G1(_)) => {
            ((cratonvm_gc::jit_satb_pre_gate_addr(), 0, 0), 0)
        }
        Some(_) => ((0, 0, 0), 0),
        _ => (
            cratonvm_gc::jit_ref_store_gate_addrs(),
            usize::from(cratonvm_gc::jit_ref_store_post_skip_mask()),
        ),
    }
}

fn build_helpers_opt(vm_for_helpers: Option<&crate::vm::SharedVm>) -> JitRuntimeHelpers {
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
    // `--jdk-only` VM in a process made every later VM compile over-strict,
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
        jit_stack_floor_offset_in_thread: if jit_stack_floor_word_enabled() {
            std::mem::offset_of!(JvmThread, jit_stack_floor)
        } else {
            0
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
/// compilation, from the threaded JDK-only policy.
pub(crate) fn direct_helper_table() -> cratonvm_jit::DirectHelperTable {
    cratonvm_jit::DirectHelperTable {
        integer_value_of: jit_integer_value_of_direct as *const () as usize,
        integer_int_value: jit_integer_int_value_direct as *const () as usize,
        long_value_of: jit_long_value_of_direct as *const () as usize,
        long_long_value: jit_long_long_value_direct as *const () as usize,
        hashmap_put: jit_hashmap_put_direct as *const () as usize,
        hashmap_get: jit_hashmap_get_direct as *const () as usize,
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
        thread_local_get: jit_thread_local_get_direct as *const () as usize,
        thread_local_set: jit_thread_local_set_direct as *const () as usize,
        monitor_enter: jit_monitor_enter as *const () as usize,
        monitor_exit: jit_monitor_exit as *const () as usize,
        preconditions_check_index: jit_preconditions_check_index_direct as *const () as usize,
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
        // unconditionally; the doors bind it only under
        // `CRATONVM_JIT_CLASS_GET_NAME_DIRECT=1`.
        class_get_name: jit_class_get_name_direct as *const () as usize,
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
        // Per VM: `direct_helper_table_for` asks
        // `thread_java_obj_offset_for` (nio20 w4 L3).
        thread_java_obj_offset: 0,
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

    // Autoboxing.
    let _: DirectHelperFnIntegerValueOf = jit_integer_value_of_direct;
    let _: DirectHelperFnIntegerIntValue = jit_integer_int_value_direct;
    let _: DirectHelperFnLongValueOf = jit_long_value_of_direct;
    let _: DirectHelperFnLongLongValue = jit_long_long_value_direct;

    // Collections.
    let _: DirectHelperFnHashMapPut = jit_hashmap_put_direct;
    let _: DirectHelperFnHashMapGet = jit_hashmap_get_direct;
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
    let _: DirectHelperFnThreadCurrentCarrierThread = jit_thread_current_carrier_thread_direct;

    // MessageDigest and `Thread.currentThread()`.
    let _: DirectHelperFnMdUpdateByte = jit_md_update_byte_direct;
    let _: DirectHelperFnThreadCurrentThread = jit_thread_current_thread_direct;

    // Monitors — the two `HandWritten` slots, reached through
    // `runtime_lowering::emit_monitor_stub`, which loads the argument
    // registers itself and has no stack-argument path at all.
    let _: DirectHelperFnMonitorEnter = jit_monitor_enter;
    let _: DirectHelperFnMonitorExit = jit_monitor_exit;
    // The deferred field site, hand-written by `op_field.rs`
    // `emit_deferred_field_site` (interpreter round i4 wave 92, lane L2).
    let _: DirectHelperFnFieldSiteCp = jit_field_site_cp;

    let _: DirectHelperFnPreconditionsCheckIndex = jit_preconditions_check_index_direct;
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
fn jit_ldc_string_body(vm_ptr: i64, bytes: *const u8, len: usize) -> i64 {
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
unsafe fn jit_safepoint_slow_path_body(flag_addr: i64, body_id: i64) -> i64 {
    static HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
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
        if hit <= 16 && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JIT_SAFEPOINTS") {
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
struct PollStressArrival {
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
fn poll_stress_arrival(vm: &SharedVm, flag_addr: i64) -> PollStressArrival {
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
fn poll_stress_arrival_from(deopt: bool, gc: bool) -> PollStressArrival {
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
pub(crate) struct PollStressCensus {
    /// Forced arrivals (the pause flag clear, a stress switch set).
    arrivals: std::sync::atomic::AtomicU64,
    /// Under deopt-a-lot: forced exits granted (the sinks' verdict, past the
    /// chain check).
    granted: std::sync::atomic::AtomicU64,
    /// Refused: the poll named no compiled body this thread runs.
    refused_unnamed: std::sync::atomic::AtomicU64,
    /// Refused: the body was withdrawn by a class redefinition.
    refused_withdrawn: std::sync::atomic::AtomicU64,
    /// Refused: the body is no `PollingBody` (an optimizing OSR body has no
    /// owner class / label to resume through).
    refused_not_polling_body: std::sync::atomic::AtomicU64,
    /// Refused: `interpreter::stress_exit_verdict` answered 0 (the body's
    /// exits cannot be resumed now, or the VM saw a redefinition).
    refused_sinks: std::sync::atomic::AtomicU64,
    /// Refused after a granted verdict: a spliced scope of the body's chain
    /// exits could not be rebuilt (`chain_poll_exits_resumable_for`).
    refused_chain: std::sync::atomic::AtomicU64,
    /// Under deopt-a-lot: another mode's verdict already sent the body out,
    /// so no forced exit was asked.
    preempted: std::sync::atomic::AtomicU64,
    /// Collections taken at a forced arrival (gc-at-back-edge).
    collections: std::sync::atomic::AtomicU64,
    /// gc-at-back-edge arrivals with no attached `JvmThread` (no collection).
    gc_no_thread: std::sync::atomic::AtomicU64,
    /// The exit line was printed (both shutdown arms may reach the report).
    printed: std::sync::atomic::AtomicBool,
}

impl PollStressCensus {
    #[inline]
    fn bump(cell: &std::sync::atomic::AtomicU64) {
        cell.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// The exit line. `refused` is the sum of the five reasons; `arrivals`
    /// also counts gc-only and preempted arrivals, so it is not
    /// `granted + refused`.
    fn census_line(&self) -> String {
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
fn poll_stress_collect(vm: &SharedVm, thread: &mut crate::threading::jvm_thread::JvmThread) {
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
unsafe fn jit_safepoint_stress_exit_verdict(vm: &SharedVm, body_id: i64) -> i64 {
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
fn osr_profit_exit_body(vm: &SharedVm, body_id: i64) -> Option<usize> {
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
fn disarm_osr_profit_exit_noted(vm: &SharedVm, cm: &cratonvm_jit::CompiledMethod, why: &str) {
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
fn chain_poll_exits_resumable_for(vm: &SharedVm, body_id: i64) -> bool {
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
fn renumbered_entry_body_resumes(vm: &SharedVm, cm: &cratonvm_jit::CompiledMethod) -> bool {
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
fn post_call_exit_transfer_ready(vm: &SharedVm, body_id: i64) -> bool {
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
/// `docs/feature-designs/jdk-only-mode.md`
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
fn bound_body_activation_entered_by_a_door(cm: &cratonvm_jit::CompiledMethod) -> bool {
    cm.named_poll_exits_only
        && cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JIT_SYNC_LEAF_DOOR_ACTIVATION_EXITS",
        )
        && crate::jit::conservative_roots::innermost_entry_names(
            cm,
            crate::jit::conservative_roots::top_rbp_mirror_read(),
        )
}

/// [`post_call_exit_transfer_ready`] for the artifact `cm` itself.
fn post_call_chain_exits_resumable(vm: &SharedVm, cm: &cratonvm_jit::CompiledMethod) -> bool {
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
    let own_sources_on = cratonvm_jit::chain_inner_own_source_enabled()
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEOPT_CHAIN_OWN_SOURCE_SINKS");
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
/// `docs/feature-designs/jdk-only-mode.md`).
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
fn interpreter_only_obsolete_source<'a>(
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
unsafe fn jit_safepoint_loop_exit_verdict(vm: &SharedVm, body_id: i64) -> i64 {
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
fn note_post_call_exit_verdict(vm: &SharedVm, body_id: i64, verdict: i64, transfer: Option<bool>) {
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
fn running_osr_body_named(id: u32) -> Option<usize> {
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
fn osr_continuation_body_named(frame_count: usize, id: u32) -> Option<usize> {
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
fn polling_body_id(body_id: i64, mirror: u32) -> Option<u32> {
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
extern "C" fn jit_frame_record(rbp: usize) {
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
extern "C" fn jit_verify_inline_frame_record(rbp: usize) {
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

/// The argument decode's two receiver routes must agree.
///
/// `try_jit_site_cached_native_dispatch` resolves the receiver to guard the site
/// and then hands the resolved `ObjectRef` to the decode instead of the raw
/// word, so the conservative header validator runs once per call rather than
/// three times. That is only sound while the two routes produce the same
/// `Value`, and the raw route has three arms the resolved one skips: null, a
/// tagged/non-canonical word, and an address the validator rejects. A
/// divergence would put a `Value::Object` the funnel then pins into a slot the
/// old route would have nulled.
#[cfg(test)]
mod decode_receiver_routes {
    use super::*;
    use crate::config::VmConfig;
    use std::sync::Arc;

    static PROBE_INFO: JitInvokeInfo = JitInvokeInfo {
        class_name: "p/Probe",
        method_name: "m",
        descriptor: "()I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    };

    #[test]
    fn a_resolved_receiver_decodes_to_what_the_raw_word_decodes_to() {
        let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let raw = obj.as_ptr() as i64;

        let mut resolved = JitDecodedArgs::new();
        let mut unresolved = JitDecodedArgs::new();
        // SAFETY: `[raw]` holds a live heap address from `alloc_object`.
        unsafe {
            decode_dispatch_values_into(
                &shared,
                &PROBE_INFO,
                &[raw],
                Some(Some(obj)),
                &mut resolved,
            );
            decode_dispatch_values_into(&shared, &PROBE_INFO, &[raw], None, &mut unresolved);
        }
        assert_eq!(resolved.len(), 1);
        assert_eq!(&resolved[..], &unresolved[..]);
        assert!(matches!(resolved[0], Value::Object(Some(o)) if o.as_ptr() == obj.as_ptr()));
    }

    /// The buffer is reused across calls, so a longer previous call must not
    /// leave arguments behind for a shorter one.
    #[test]
    fn a_reused_buffer_does_not_leak_the_previous_calls_arguments() {
        let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        static TWO_INT_INFO: JitInvokeInfo = JitInvokeInfo {
            class_name: "p/Probe",
            method_name: "m",
            descriptor: "(II)I",
            num_jit_args: 3,
            return_type: b'I',
            invoke_kind: 0,
            declaring_class_id: 0,
            owner_class_id: 0,
        };
        let mut buf = JitDecodedArgs::new();
        // SAFETY: slot 0 is a live heap address; the rest are plain ints.
        unsafe {
            decode_dispatch_values_into(
                &shared,
                &TWO_INT_INFO,
                &[obj.as_ptr() as i64, 7, 9],
                Some(Some(obj)),
                &mut buf,
            );
            assert_eq!(buf.len(), 3);
            decode_dispatch_values_into(
                &shared,
                &PROBE_INFO,
                &[obj.as_ptr() as i64],
                Some(Some(obj)),
                &mut buf,
            );
        }
        assert_eq!(
            buf.len(),
            1,
            "the reused buffer still holds the previous call's arguments"
        );
    }
}

/// Where a **compiled-code** native call's fixed cost goes, step by step.
///
/// `native-funnel-fixed-cost-is-the-remaining-wall-RETIRED-20260806.md` bounded the
/// cost from Java and then said so itself: *"Nobody has profiled it. This
/// document asserts a bound, not a line."* Its companion,
/// `native-call-funnel-per-call-floor-item2-20260805.md`, profiled
/// `safe_native_call_impl`'s **body** (`vm_exec.rs`, `native_funnel_profile`)
/// and found it to be ~23-30 ns for one argument. That leaves the larger half
/// of a compiled native call unmeasured: everything `jit_invoke_dispatch` and
/// [`try_jit_site_cached_native_dispatch`] do *around* the funnel — the
/// boundary bookkeeping, the memo flushes, the receiver guard, the argument
/// decode and the return coercion.
///
/// This module times those, one at a time, in one process against a bare
/// [`SharedVm`] with a real heap object as the receiver. `#[ignore]`d because
/// it is a measurement, not an assertion — a timing threshold here would be a
/// flake on a shared build host. Run it:
///
/// ```text
/// cargo test --release -p cratonvm-vm --lib jit_native_dispatch -- --ignored --nocapture
/// ```
///
/// Multi-pass by construction: a rung that has not gone flat is not a
/// measurement.
#[cfg(test)]
mod jit_native_dispatch_profile {
    use super::*;
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use std::hint::black_box;
    use std::sync::Arc;
    use std::time::Instant;

    const PASSES: usize = 4;
    const ROUNDS: u32 = 400_000;

    fn noop_native(
        _ctx: &mut dyn cratonvm_native_api::NativeContext,
        _args: &[Value],
    ) -> crate::error::MethodCallResult {
        Ok(None)
    }

    fn rung(label: &str, mut f: impl FnMut()) {
        print!("{label:<56}");
        for _ in 0..PASSES {
            let t0 = Instant::now();
            for _ in 0..ROUNDS {
                f();
            }
            let ns = t0.elapsed().as_nanos() as f64 / f64::from(ROUNDS);
            print!("{ns:>10.1}");
        }
        println!();
    }

    /// An `AtomicInteger.get()`-shaped site: instance receiver, no arguments.
    static GET_INFO: JitInvokeInfo = JitInvokeInfo {
        class_name: "java/util/concurrent/atomic/AtomicInteger",
        method_name: "get",
        descriptor: "()I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    };

    /// A `setExclusiveOwnerThread(Thread)`-shaped site: receiver + one object.
    static SET_OWNER_INFO: JitInvokeInfo = JitInvokeInfo {
        class_name: "java/util/concurrent/locks/AbstractOwnableSynchronizer",
        method_name: "setExclusiveOwnerThread",
        descriptor: "(Ljava/lang/Thread;)V",
        num_jit_args: 2,
        return_type: b'V',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    };

    /// A `System.nanoTime()`-shaped site: static, no arguments.
    static NANO_INFO: JitInvokeInfo = JitInvokeInfo {
        class_name: "java/lang/System",
        method_name: "nanoTime",
        descriptor: "()J",
        num_jit_args: 0,
        return_type: b'J',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    };

    #[test]
    #[ignore = "measurement, not an assertion — see the module doc"]
    fn compiled_native_call_step_breakdown() {
        let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "jit-dispatch-profile");
        let recv = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let other = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let recv_raw = recv.as_ptr() as i64;
        let other_raw = other.as_ptr() as i64;
        let one_arg: [i64; 1] = [recv_raw];
        let two_args: [i64; 2] = [recv_raw, other_raw];
        let no_args: [i64; 0] = [];
        let cb: cratonvm_native_api::NativeCallback = noop_native;
        let key = jit_site_key(shared.vm_identity, &GET_INFO as *const _ as usize);

        // Seed the site cache so the probe below measures a HIT, which is the
        // steady state every one of these numbers is about.
        NATIVE_SITE_CACHE.with(|c| {
            c.borrow_mut().insert(
                key,
                (
                    shared.natives.native_methods.generation(),
                    Some(NativeSiteCache {
                        kind: LeafNativeKind::Callback,
                        leaf: true,
                        callback: cb,
                        native_id: None,
                        receiver_class_id: Some(0),
                        // The rungs below price an UNCLASSIFIED native, which
                        // is what the cache overwhelmingly serves.
                        capability_kind: None,
                        desc: NativeSiteDescriptor::decode(GET_INFO.descriptor),
                        // These rungs price the ORDINARY native dispatch. A
                        // signature-polymorphic entry takes a different tail
                        // (`try_varhandle_instance_field_read`), so setting
                        // this would make the numbers about something else.
                        poly: false,
                        vh_value_site: 0,
                        sync: None,
                        native_owner: None,
                        fill_redefine_epoch: 0,
                        owner_scoped: false,
                        refused_for_receiver: false,
                    }),
                ),
            );
        });

        print!("{:<56}", "rung");
        for i in 1..=PASSES {
            print!("{i:>10}");
        }
        println!("   (ns/op per pass; read the LAST)");

        // --- the receiver validator, and how many times it is paid ---------
        rung("heap.is_object_address(receiver)", || {
            black_box(shared.mem.heap.is_object_address(recv_raw as usize));
        });
        rung("heap.class_id_of(receiver)", || {
            black_box(shared.mem.heap.class_id_of(recv));
        });
        rung("heap.class_id_of_validated(receiver)", || {
            black_box(shared.mem.heap.class_id_of_validated(recv));
        });
        rung("heap.load_and_forward(receiver)", || {
            black_box(shared.mem.heap.load_and_forward(recv));
        });

        println!();

        // --- jit_invoke_dispatch's preamble, per step ----------------------
        rung("note_jit_boundary()", || {
            crate::jit::conservative_roots::note_jit_boundary();
        });
        rung("heap.flush_thread_satb()", || {
            shared.mem.heap.flush_thread_satb();
        });
        rung("mic_prof::dump_maybe_disp()", || {
            mic_prof::dump_maybe_disp();
        });
        rung("env_cache::disable_jit()", || {
            black_box(crate::runtime::env_cache::disable_jit());
        });
        rung("forward_jit_reference_args: 1 recv, ()I", || {
            black_box(forward_jit_reference_args(&shared, &GET_INFO, &one_arg));
        });
        rung("forward_jit_reference_args: recv+obj, (LThread;)V", || {
            black_box(forward_jit_reference_args(
                &shared,
                &SET_OWNER_INFO,
                &two_args,
            ));
        });
        rung("flush_raw_entry_dispatch_caches()", || {
            flush_raw_entry_dispatch_caches();
        });
        rung("jit_site_key()", || {
            black_box(jit_site_key(
                shared.vm_identity,
                &GET_INFO as *const _ as usize,
            ));
        });
        rung("site_alias_detect_enabled() gate [off]", || {
            if black_box(site_alias_detect_enabled()) {
                note_site_identity(key, &GET_INFO);
            }
        });
        // What the gate BUYS, on the one call site that used to lack it
        // (`jit_invoke_virtual_mic`, fixed by `5d299ca6f`). This is the
        // steady state, and the CHEAPEST case: the key is already in the map
        // and still names the same site, so it is one thread-local access,
        // one `FxHashMap` lookup and three `String` comparisons, with no
        // insert and no `eprintln!`. The very first call for a key is dearer
        // — it allocates and stores three `String`s in a map that is never
        // bounded and never cleared — and a key that HAS been recycled costs
        // that again plus the event line. Seed the entry first, or this
        // measures the insert on pass 1 and the hit on passes 2..N and goes
        // flat at neither.
        note_site_identity(key, &GET_INFO);
        rung("note_site_identity() UNGUARDED [warm hit]", || {
            note_site_identity(black_box(key), &GET_INFO);
        });
        rung("OBJECT_NATIVE_DISPATCH_CACHE probe (miss)", || {
            black_box(OBJECT_NATIVE_DISPATCH_CACHE.with(|c| c.borrow().get(&key).copied()));
        });

        println!();

        // --- try_jit_site_cached_native_dispatch, per step -----------------
        rung("native_site_cache_enabled()", || {
            black_box(native_site_cache_enabled());
        });
        rung("native_methods.generation()", || {
            black_box(shared.natives.native_methods.generation());
        });
        rung("NATIVE_SITE_CACHE probe (hit)", || {
            black_box(NATIVE_SITE_CACHE.with(|c| c.borrow().get(&key).copied()));
        });
        rung("class_id_or_name_was_redefined()", || {
            black_box(class_id_or_name_was_redefined(
                &shared,
                GET_INFO.declaring_class_id,
                GET_INFO.class_name,
            ));
        });
        rung("class_was_redefined()", || {
            black_box(class_was_redefined(&shared, ClassId::new(0)));
        });
        rung("jit_thread_mut()", || {
            // SAFETY: measurement-only; no JIT thread is installed, so this
            // takes the null arm — which is exactly the TLS read being priced.
            black_box(unsafe { jit_thread_mut() }.is_some());
        });
        rung("decode_dispatch_values: 1 recv, ()I", || {
            // SAFETY: `one_arg` holds a validated heap address for `recv`.
            black_box(unsafe { decode_dispatch_values(&shared, &GET_INFO, &one_arg) });
        });
        rung("decode_dispatch_values: recv+obj, (LThread;)V", || {
            // SAFETY: both slots hold validated heap addresses.
            black_box(unsafe { decode_dispatch_values(&shared, &SET_OWNER_INFO, &two_args) });
        });
        rung("decode_dispatch_values: static, ()J", || {
            // SAFETY: an empty slice is trivially in-bounds.
            black_box(unsafe { decode_dispatch_values(&shared, &NANO_INFO, &no_args) });
        });
        let mut buf = JitDecodedArgs::new();
        rung("decode_..._into: 1 recv (resolved), ()I", || {
            // SAFETY: `one_arg[0]` is `recv`, already validated above.
            unsafe {
                decode_dispatch_values_into(
                    &shared,
                    &GET_INFO,
                    &one_arg,
                    Some(Some(recv)),
                    &mut buf,
                )
            };
            black_box(&mut buf);
        });
        rung("decode_..._into: recv+obj (resolved), (LThread;)V", || {
            // SAFETY: both slots hold validated heap addresses.
            unsafe {
                decode_dispatch_values_into(
                    &shared,
                    &SET_OWNER_INFO,
                    &two_args,
                    Some(Some(recv)),
                    &mut buf,
                )
            };
            black_box(&mut buf);
        });
        rung("decode_..._into: static, ()J", || {
            // SAFETY: an empty slice is trivially in-bounds.
            unsafe { decode_dispatch_values_into(&shared, &NANO_INFO, &no_args, None, &mut buf) };
            black_box(&mut buf);
        });

        rung("record_invocation() [no id]", || {
            count_jit_native_dispatch(&shared, None);
        });
        // The three DIAGNOSTIC counters every site-cached native dispatch pays,
        // priced separately because the rung above takes the `None` arm and so
        // measures nothing. The first two are per-thread `JitCounterBlock`
        // load+store bumps since round 11 (no `lock` prefix); only
        // `record_invocation` with a real id is still a shared `lock xadd`
        // (`native-api` registry, `r11-rt-helper-hot-paths` item 3).
        rung("counter: note_membership_walk(1)", || {
            note_membership_walk(1);
        });
        rung("counter: leaf-hit tally (per-thread block)", || {
            note_leaf_native_hit();
        });
        {
            let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
            registry.with_category(cratonvm_native_api::NativeKind::Bridge, |r| {
                r.register("p/P", "m", "()V", noop_native);
            });
            let id = registry.resolve_id("p/P", "m", "()V");
            rung("counter: record_invocation() [real id]", || {
                if let Some(id) = id {
                    registry.record_invocation(id);
                }
            });
        }
        rung("counter: all three, as one dispatch pays them", || {
            note_membership_walk(1);
            note_leaf_native_hit();
            count_jit_native_dispatch(&shared, None);
        });
        rung("counter: both JIT-side tallies, one TLS access", || {
            JIT_COUNTERS.with(|b| {
                JitCounterBlock::bump(&b.membership_walk[1]);
                JitCounterBlock::bump(&b.leaf_native_hits);
            });
        });
        rung("coerce_native_return(Int, \"()I\")", || {
            black_box(crate::vm::coerce_native_return(
                Some(Value::Int(3)),
                GET_INFO.descriptor,
            ));
        });

        println!();

        // --- the two call wrappers, for scale ------------------------------
        let recv_val = [Value::Object(Some(recv))];
        rung("safe_native_call_leaf: 1 object arg", || {
            black_box(crate::vm::safe_native_call_leaf(
                &shared,
                &mut thread,
                cb,
                &recv_val,
            ))
            .ok();
        });
        rung("safe_native_call_prevalidated: 1 object arg", || {
            black_box(crate::vm::safe_native_call_prevalidated_objects(
                &shared,
                &mut thread,
                cb,
                &recv_val,
            ))
            .ok();
        });
        let two_vals = [Value::Object(Some(recv)), Value::Object(Some(other))];
        rung("safe_native_call_prevalidated: 2 object args", || {
            black_box(crate::vm::safe_native_call_prevalidated_objects(
                &shared,
                &mut thread,
                cb,
                &two_vals,
            ))
            .ok();
        });

        println!();

        // --- the one native body on the AQS path that is not a field write ---
        // `setExclusiveOwnerThread` is intercepted for `ThreadMXBean`, not for
        // speed, and the interception's whole cost is this index update: a
        // global mutex over a `HashMap`, the thread-registry `RwLock`, and a
        // per-thread `Mutex` over a `Vec`. It runs twice per uncontended
        // `ReentrantLock.lock()`/`unlock()` pair.
        let registry = &shared.threads.thread_registry;
        rung(
            "ThreadRegistry::set_jmx_owned_synchronizer (acquire+release)",
            || {
                registry.set_jmx_owned_synchronizer(
                    Some(crate::threading::jvm_thread::ThreadId(0)),
                    recv,
                );
                registry.set_jmx_owned_synchronizer(None, recv);
            },
        );
        // The own-handle form, for the same acquire/release pair. A bare
        //  has no registered thread, so the handle is built here
        // rather than fetched — which is what the owning thread holds.
        let own_list: parking_lot::Mutex<Vec<ObjectRef>> = parking_lot::Mutex::new(Vec::new());
        rung("  ... _own (acquire+release, no registry RwLock)", || {
            registry.set_jmx_owned_synchronizer_own(
                crate::threading::jvm_thread::ThreadId(0),
                &own_list,
                Some(crate::threading::jvm_thread::ThreadId(0)),
                recv,
            );
            registry.set_jmx_owned_synchronizer_own(
                crate::threading::jvm_thread::ThreadId(0),
                &own_list,
                None,
                recv,
            );
        });
    }
}

/// The JIT's `monitorenter`/`monitorexit` helpers must leave the thread's JMX
/// owned-monitor set exactly as they found it.
///
/// This is a performance test wearing a correctness test's clothes, and the
/// correctness half is real: `getLockedMonitors()` must not name a monitor the
/// thread released, and the list is a GC root set, so a stale entry pins its
/// object for the life of the thread.
///
/// The performance half is why it exists. `ThreadRegistry::jmx_locked_monitors`
/// is **membership-scanned linearly on every acquisition**, so a publish with no
/// matching retract does not cost one wasted slot — it grows the list to every
/// distinct object the thread has ever locked and makes `monitorenter` O(that).
/// `jit_monitor_exit` released the monitor and stopped there, and on
/// `ZipContentTests` the two scan functions measured **54% of the whole JIT run**
/// — the bulk of the JIT's CPU deficit against `--nojit` on lock-dense code.
///
/// The same defect had already been found and fixed once, at a different exit
/// site, where it was 14.9% of a Tomcat webapp deploy. Two occurrences of one
/// shape is what makes this a test rather than a comment.
#[cfg(test)]
mod jit_monitor_jmx_pairing {
    use super::*;
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use std::sync::Arc;

    /// Addresses of the monitors the registry currently believes `tid` owns.
    fn owned(shared: &SharedVm, tid: ThreadId) -> Vec<usize> {
        shared
            .threads
            .thread_registry
            .jmx_lock_snapshot(tid)
            .map(|snap| snap.2.iter().map(|o| o.as_ptr() as usize).collect())
            .unwrap_or_default()
    }

    /// Boot a VM with one registered thread whose JIT thread-pointer is
    /// installed, so `jit_monitor_enter`/`jit_monitor_exit` can be called
    /// exactly as generated code calls them.
    fn vm_with_jit_thread() -> (Arc<SharedVm>, Box<JvmThread>, ThreadId) {
        let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let tid = ThreadId(1);
        let thread = Box::new(JvmThread::new(tid, "jit-monitor-probe"));
        shared
            .threads
            .thread_registry
            .register(tid, "jit-monitor-probe", None);
        (shared, thread, tid)
    }

    #[test]
    fn the_jit_helpers_publish_and_retract_the_same_monitor() {
        let (shared, mut thread, tid) = vm_with_jit_thread();
        let vm_ptr = Arc::as_ptr(&shared) as i64;
        let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);

        let scope = set_jit_thread(&mut thread);
        // SAFETY: `vm_ptr` names the live `SharedVm` above, `obj` is a live
        // heap object, and the JIT thread pointer is installed for this OS
        // thread — the exact contract compiled code satisfies.
        unsafe {
            assert_ne!(
                jit_monitor_enter(vm_ptr, obj.as_ptr() as i64),
                i64::MIN,
                "the helper failed to acquire an uncontended monitor"
            );
            // Prove the RED first: without a publish here there would be
            // nothing for the retract to remove, and the assertion after the
            // exit would pass on a helper that does no bookkeeping at all.
            assert_eq!(
                owned(&shared, tid),
                vec![obj.as_ptr() as usize],
                "a compiled monitorenter must publish JMX ownership"
            );
            assert_ne!(
                jit_monitor_exit(vm_ptr, obj.as_ptr() as i64),
                i64::MIN,
                "the helper failed to release a monitor it holds"
            );
        }
        restore_jit_thread(scope);

        assert!(
            owned(&shared, tid).is_empty(),
            "a compiled monitorexit left its JMX ownership publish standing — \
             the owned-monitor list is scanned on every acquisition, so this is \
             an unbounded list and a quadratic monitorenter, not one stale slot"
        );
    }

    /// The failure mode is about DISTINCT objects, and one balanced pair cannot
    /// see it: the leak only becomes quadratic once the list has more entries
    /// than the thread's real lock nesting depth.
    #[test]
    fn locking_many_distinct_objects_leaves_no_residue() {
        const OBJECTS: usize = 64;
        let (shared, mut thread, tid) = vm_with_jit_thread();
        let vm_ptr = Arc::as_ptr(&shared) as i64;
        let objs: Vec<_> = (0..OBJECTS)
            .map(|_| shared.mem.heap.alloc_object(ClassId::new(0), 0))
            .collect();

        let scope = set_jit_thread(&mut thread);
        // SAFETY: as above; every ref comes from this heap and is still live.
        unsafe {
            for obj in &objs {
                jit_monitor_enter(vm_ptr, obj.as_ptr() as i64);
                jit_monitor_exit(vm_ptr, obj.as_ptr() as i64);
            }
        }
        restore_jit_thread(scope);

        assert_eq!(
            owned(&shared, tid).len(),
            0,
            "{OBJECTS} balanced compiled lock/unlock pairs left {} monitors \
             recorded as owned; each one is a permanent GC root and a permanent \
             entry in the scan every subsequent monitorenter runs",
            owned(&shared, tid).len()
        );
    }

    /// A re-entrant acquisition is still held after the inner release, and
    /// retracting there would under-report a monitor the thread really owns.
    /// The retract is conditional on `holds` precisely for this case.
    #[test]
    fn a_reentrant_release_keeps_the_publish_until_the_outermost_exit() {
        let (shared, mut thread, tid) = vm_with_jit_thread();
        let vm_ptr = Arc::as_ptr(&shared) as i64;
        let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let addr = obj.as_ptr() as i64;

        let scope = set_jit_thread(&mut thread);
        // SAFETY: as above.
        unsafe {
            jit_monitor_enter(vm_ptr, addr);
            jit_monitor_enter(vm_ptr, addr);
            jit_monitor_exit(vm_ptr, addr);
        }
        let after_inner = owned(&shared, tid);
        // SAFETY: as above.
        unsafe {
            jit_monitor_exit(vm_ptr, addr);
        }
        let after_outer = owned(&shared, tid);
        restore_jit_thread(scope);

        assert_eq!(
            after_inner,
            vec![addr as usize],
            "the inner release of a re-entrant lock retracted a monitor the \
             thread still holds"
        );
        assert!(
            after_outer.is_empty(),
            "the outermost release left the publish standing"
        );
    }

    /// gcd d1/mon: the entry screen of both monitor helpers. A stack address
    /// (what the optimizing tier passed when it loaded an unreserved context
    /// slot, `[rbp - 0]`), null, the first page and a misaligned word are
    /// refused; a live `SharedVm` -- always an `Arc` on the Rust heap -- is
    /// admitted. The refusal itself aborts, so only the predicate is run here.
    #[test]
    fn the_monitor_helpers_screen_a_stack_address_posing_as_the_vm() {
        let (shared, _thread, _tid) = vm_with_jit_thread();
        let vm_ptr = Arc::as_ptr(&shared) as i64;
        assert!(
            !jit_monitor_vm_ptr_is_wild(vm_ptr),
            "a live SharedVm must pass the screen"
        );
        let on_stack = 0u64;
        let stack_addr = std::ptr::addr_of!(on_stack) as usize as i64;
        if cratonvm_gc::shadow_stack::current_thread_stack().is_some() {
            assert!(
                jit_monitor_vm_ptr_is_wild(stack_addr),
                "an address in this thread's own stack is never a SharedVm"
            );
        }
        assert!(jit_monitor_vm_ptr_is_wild(0));
        assert!(jit_monitor_vm_ptr_is_wild(0x251));
        assert!(jit_monitor_vm_ptr_is_wild(vm_ptr + 4));
    }
}

/// The compiled tier asks exactly one type question about an `aastore`, and
/// this pins which question it is.
///
/// `RArrayStoreInterfaces` s12 (`Runnable[] <- Proxy(Marker)`) was MEASURED
/// divergent against HotSpot 25.0.3+9-LTS in `cold` AND `hot` — the same wrong
/// answer in both tiers. That is only possible because both tiers route the
/// decision to one predicate: the interpreter's `aastore` opcode calls
/// `aastore_element_assignable` directly, and every compiled `aastore`
/// ([`jit_aastore`] and the x64 inline lowering's [`jit_aastore_type_check`])
/// reaches it through the single call in [`aastore_store_is_refused`]. So a fix
/// in `typecheck.rs` moves both, and no edit in this file was needed.
///
/// That claim is worth an assertion rather than a comment, because this file
/// has already been the site of the opposite: R20/HIGH-5 replaced the helper
/// call with an inline store on the strength of a comment that was true when
/// written and silently false afterwards (`RExceptions`: `cold=[java.lang.Integer]
/// hot=[no-throw]`). A premise in a comment is not a compile-time link.
///
/// docs/feature-designs/concurrent-gc-maturation.md
#[cfg(test)]
mod g12_compiled_aastore_asks_the_shared_predicate {
    use super::*;
    use crate::config::VmConfig;
    use cratonvm_classloading::ClassOrigin;
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use std::sync::Arc;

    /// Fabricate an INTERFACE.
    ///
    /// A plain `fn`, not a closure: the guard must be dropped before the next
    /// statement (`aastore_element_assignable` takes the read lock itself, and a
    /// live write guard would deadlock the test rather than fail it), and the
    /// `Arc<SharedVm>` is moved into the returned tuple below, which a closure
    /// borrowing it would complicate for no gain.
    fn mk_iface(shared: &SharedVm, name: &str) -> ClassId {
        let mut cm = shared.classes.class_manager.write();
        let id = cm
            .try_ensure_synthetic_class(name, 0)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
        cm.class_store
            .get_mut(id)
            .expect("just fabricated")
            .access_flags |= ClassAccessFlags::INTERFACE;
        id
    }

    /// One VM, two unrelated fabricated interfaces, and a `$ProxyN` whose
    /// RECORDED interface set is `[G12JitMarker]` — the shape
    /// `define_or_get_proxy_class` produces from real `proxy_gen` bytes.
    ///
    /// The proxy class is deliberately given no `interfaces` vector and no
    /// proxy superclass, so the hierarchy walk inside the predicate declines for
    /// it: whatever these assertions observe comes from the recorded interface
    /// set, not from the class graph.
    ///
    /// Returns `(vm, G12JitMarker[], G12JitOther[], the proxy)`.
    fn vm_with_a_recorded_proxy() -> (Arc<SharedVm>, ObjectRef, ObjectRef, ObjectRef) {
        let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let marker = mk_iface(&shared, "cratonvm/test/G12JitMarker");
        let other = mk_iface(&shared, "cratonvm/test/G12JitOther");
        let proxy_cid = {
            let mut cm = shared.classes.class_manager.write();
            let id = cm
                .try_ensure_synthetic_class("jdk/proxy12/$Proxy0", 0)
                .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
            cm.class_store
                .get_mut(id)
                .expect("just fabricated")
                .set_origin(ClassOrigin::GeneratedProxy {
                    interfaces: Arc::from(vec![marker]),
                });
            id
        };
        // A reference array's own class id IS its component class id
        // (JVMS §4.4.1).
        let marker_arr = shared
            .mem
            .heap
            .alloc_array(marker, ArrayElementType::Reference, 1);
        let other_arr = shared
            .mem
            .heap
            .alloc_array(other, ArrayElementType::Reference, 1);
        let proxy = shared.mem.heap.alloc_object(proxy_cid, 0);
        (shared, marker_arr, other_arr, proxy)
    }

    /// The LEGAL direction, end to end through the extern entry point compiled
    /// code actually calls. `s25` on the fixture, and the row that goes red if
    /// the fix were phrased as "refuse every proxy" — which is how this vector
    /// goes from 2 fails to 20.
    ///
    /// It needs no exception machinery at all (a `0` is the allow path), which
    /// is why the legal direction is the one asserted here and the refusal is
    /// asserted at the predicate below: reaching `i64::MIN` requires an
    /// installed JIT thread AND a throwable built through
    /// `throw_runtime_error`, i.e. two mechanisms that are `RExceptions`'
    /// subject and not this one's.
    #[test]
    fn the_compiled_entry_point_admits_a_proxy_into_its_own_interfaces_array() {
        let (shared, marker_arr, _other_arr, proxy) = vm_with_a_recorded_proxy();
        let vm_ptr = Arc::as_ptr(&shared) as i64;
        // SAFETY: `vm_ptr` names the live `SharedVm` above and both refs are
        // live heap objects from its own heap — the contract compiled code
        // satisfies.
        unsafe {
            assert_eq!(
                jit_aastore_type_check(vm_ptr, marker_arr.as_ptr() as i64, proxy.as_ptr() as i64),
                0,
                "a proxy generated for G12JitMarker must still store into a \
                 G12JitMarker[] in COMPILED code"
            );
            assert_eq!(
                jit_aastore_type_check(vm_ptr, marker_arr.as_ptr() as i64, 0),
                0,
                "a null element is always storable and must never reach the \
                 predicate at all"
            );
        }
    }

    /// The REFUSED direction, at the predicate `aastore_store_is_refused`
    /// consults two lines in. Everything after that call in this file is
    /// message construction and throwable delivery — the *decision* is here,
    /// and it is the same `false` the interpreter's `aastore` arm reads.
    #[test]
    fn the_predicate_the_compiled_path_consults_refuses_the_measured_row() {
        let (shared, _marker_arr, other_arr, proxy) = vm_with_a_recorded_proxy();
        assert!(
            !crate::runtime::interpreter::aastore_element_assignable(&shared, other_arr, proxy),
            "`Runnable[] <- Proxy(Marker)` is ArrayStoreException on HotSpot \
             25.0.3 (MEASURED, RArrayStoreInterfaces s12) in both tiers; the \
             compiled tier reads this predicate and nothing else"
        );
    }
}

// ===========================================================================
// H20-1 — the source witness for an enforcement that has since landed.
//
// This block used to read: *"Making the nine raw helper items
// `pub(in crate::jit)` is what would make a door that skips
// `admit_direct_native_entry` a compile error … lane H20 owned neither file
// and was forbidden to build. Until O1/O2 land, this is the guard: a ladder
// that takes a raw address is a RED TEST … A frozen count, not a ban,
// precisely because the nine existing takes are legitimate-for-now."*
//
// `H12-1` O1/O2 landed on 2026-09-22. The thirty-one items ARE
// `pub(in crate::jit)`, both ladders are converted, and the frozen count is
// zero at both doors — so the compile error the block describes is the
// enforcement now, and these tests are the second line rather than the only
// one.
//
// Kept, and re-aimed at the thing a compile error cannot catch: a helper item
// widened back to `pub` on the way to a thirty-second door. That widening
// breaks nothing until a ladder misuses it, and by then it is old.
// `a-premise-in-a-comment-is-not-a-compile-time-link` is the defect class this
// whole lane is about, and "do not widen these" written in a comment would be
// another instance of it.
// ===========================================================================
// ===========================================================================
// The callee-side gate's own witnesses.
//
// `jdkonly-thin-jit-direct-helpers-refuse-what-the-tail-then-runs-20260922`
// is a page about a refusal that refused nothing, and the reason it survived
// two audits is that NOTHING OBSERVABLE CHANGED: the same native ran either
// way. A behavioural test could not have caught it and cannot guard it. What
// these pin instead are the properties whose loss would re-open it -- the
// decision is one predicate, that predicate asks `invoke_or_native`'s
// question, and every gate's capability field is the one `classify_native`
// actually answers.
// ===========================================================================
#[cfg(test)]
mod direct_helper_gate_witness {
    use super::*;

    /// A gate's capability field must be `classify_native`'s own answer.
    ///
    /// The field exists so the per-call path does not run a string `match`
    /// (see [`DirectHelperGate::capability`]); a hand-written copy of an
    /// answer is exactly the drift
    /// `a-serviceability-predicate-must-mirror-the-dispatch-it-guards` is
    /// about, so the copy is checked against the source rather than against a
    /// reading of it.
    #[test]
    fn every_direct_helper_gate_mirrors_classify_native() {
        for (name, gate) in ALL_DIRECT_HELPER_GATES {
            assert_eq!(
                gate.capability,
                cratonvm_native_api::capability::classify_native(
                    gate.class_name(),
                    gate.method_name()
                ),
                "{name} states a capability its triple does not have (or misses \
                 one it does): the thin helpers bypass the funnel's \
                 `check_native_dispatch_capability`, so this field is the only \
                 thing standing between a classified native and a compiled \
                 fast path with no gate",
            );
        }
    }

    /// And today every one of them is `None` -- which is the page's second
    /// bullet, answered.
    ///
    /// A SEPARATE assertion from the mirror above, on purpose. The mirror says
    /// the field is honest; this says what the honest answer currently is, and
    /// therefore that the capability trade the page refuses to make quietly
    /// **is not being made**. `jdk/internal/misc/Unsafe` and
    /// `java/lang/foreign/MemorySegment` are `RawMemory`, but no gate declared
    /// here names either, and `java/nio/*` -- the Panama / `DirectByteBuffer`
    /// accessors the page worried about -- is not in `classify_native`'s table
    /// at all. So `check_native_dispatch_capability` is a guaranteed `Ok(())`
    /// for every triple these helpers stand in front of, and admitting them
    /// takes nothing off.
    ///
    /// When this goes red, the gate it names is the one where the trade has to
    /// be decided explicitly rather than inherited.
    #[test]
    fn only_the_ten_unsafe_gates_are_capability_classified() {
        // The trade the module block above says is "not being made" for
        // every OTHER gate: these eight are, deliberately, and this test is
        // what turns that from a reading of the source into an assertion.
        // `docs/feature-designs/jdk-only-mode.md`
        // is where the decision (RawMemory, checked in every mode, not
        // narrowed) was made explicit.
        let mut classified: Vec<&str> = ALL_DIRECT_HELPER_GATES
            .iter()
            .filter(|(_, gate)| gate.capability.is_some())
            .map(|(name, _)| *name)
            .collect();
        classified.sort_unstable();
        assert_eq!(
            classified,
            vec![
                "UNSAFE_CAS_INT_GATE",
                "UNSAFE_CAS_LONG_GATE",
                // nio21 wave 3, lane L4: seven more `jdk/internal/misc/Unsafe`
                // rows, `RawMemory` and checked in every mode like the rest.
                "UNSAFE_CAS_REFERENCE_GATE",
                "UNSAFE_FULL_FENCE_GATE",
                "UNSAFE_GET_BYTE_GATE",
                "UNSAFE_GET_CHAR_GATE",
                "UNSAFE_GET_INT_GATE",
                "UNSAFE_GET_INT_VOLATILE_GATE",
                "UNSAFE_GET_LONG_GATE",
                "UNSAFE_GET_LONG_VOLATILE_GATE",
                "UNSAFE_GET_REFERENCE_GATE",
                "UNSAFE_GET_REFERENCE_VOLATILE_GATE",
                "UNSAFE_GET_SHORT_GATE",
                "UNSAFE_PUT_BYTE_GATE",
                "UNSAFE_PUT_CHAR_GATE",
                "UNSAFE_PUT_INT_GATE",
                "UNSAFE_PUT_INT_VOLATILE_GATE",
                "UNSAFE_PUT_LONG_GATE",
                "UNSAFE_PUT_LONG_VOLATILE_GATE",
                "UNSAFE_PUT_REFERENCE_VOLATILE_GATE",
                "UNSAFE_PUT_SHORT_GATE",
            ],
            "the set of capability-classified gates changed. If a NEW one \
             joined the eight `Unsafe` accessors, confirm `jit_direct_helper_refused` \
             and `direct_native_bind_admitted` still check it unconditionally \
             of `--jdk-only` before adding it here; if one of the eight left, \
             `classify_native` narrowed and this list (and the page) should \
             say so.",
        );
        for (name, gate) in ALL_DIRECT_HELPER_GATES {
            if classified.contains(name) {
                assert_eq!(
                    gate.capability,
                    Some(cratonvm_native_api::CapabilityKind::RawMemory),
                    "{name} is capability-classified but not as RawMemory",
                );
            }
        }
    }

    /// Two gates must not share a triple.
    ///
    /// A shared triple is the `NativeCallSite` misuse its own doc names, one
    /// level up: the memo is per gate, so two helpers over one gate would be
    /// correct only by accident, and two gates over one triple means one of
    /// them is asking about a native it does not re-implement. The
    /// `ByteBuffer` pair is the near miss -- `dbb_direct_elem_addr` is shared
    /// between `get` and `put`, and a single gate there would police the read
    /// about `put`.
    #[test]
    fn every_direct_helper_gate_polices_its_own_triple() {
        let mut seen: Vec<((&str, &str, &str), &str)> = Vec::new();
        for (name, gate) in ALL_DIRECT_HELPER_GATES {
            let triple = gate.triple();
            if let Some((_, other)) = seen.iter().find(|(t, _)| *t == triple) {
                panic!("{name} and {other} both police {triple:?}");
            }
            seen.push((triple, name));
        }
        assert_eq!(seen.len(), ALL_DIRECT_HELPER_GATES.len());
    }

    /// Every declared gate is consulted, and consulted by name.
    ///
    /// A gate nobody passes to [`jit_direct_helper_refused`] is a policy
    /// question that is never asked -- the `check-orphan-instruments.sh`
    /// shape, and indistinguishable at run time from a gate that always
    /// admits.
    #[test]
    fn every_direct_helper_gate_is_consulted() {
        let src = crate::jit::helpers::HELPERS_SRC;
        for (name, _) in ALL_DIRECT_HELPER_GATES {
            let uses = src.matches(&format!("&{name}")).count();
            assert!(
                uses >= 1,
                "{name} is declared and never consulted: a gate that no helper \
                 passes to `jit_direct_helper_refused` is a policy question \
                 nobody asks",
            );
        }
    }

    /// The gate asks the tail's question, in the tail's terms.
    ///
    /// The sibling of `jit_fast_native_admission_asks_the_tails_question`, and
    /// a source witness for the same reason that one is: the defect is that a
    /// refusal here routes the call to `vm_exec::invoke_or_native`, which then
    /// runs the very native the helper refused to re-implement. The bytes that
    /// came out were identical. What changed was ~3x on the loop the page
    /// measures, and a counter that reported strict mode blocking something it
    /// had not blocked.
    #[test]
    fn the_direct_helper_gate_asks_the_tails_question() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let start = src
            .find("fn direct_helper_refusal(")
            .expect("direct_helper_refusal must exist");
        let after = &src[start..];
        let end = after[1..]
            .find("\n/// The one sanctioned way for a compile door")
            .expect("`admit_direct_native_entry`'s doc comment must follow it")
            + 1;
        let decision = &after[..end];
        assert!(
            decision.contains("jit_fast_native_bytecode_terms("),
            "the step-3 input must come from the predicate that mirrors \
             `invoke_or_native`, or the guard refuses a Bridge the tail then \
             runs -- which is the whole defect",
        );
        assert!(
            !decision.contains("jit_fast_native_has_bytecode("),
            "the raw hierarchy probe is NOT the tail's question: \
             `invoke_or_native` computes it only for a SyntheticStub on an \
             allow-listed class",
        );
        assert!(
            decision.contains("NativeKind::SyntheticStub) => true"),
            "a fabrication must still be refused outright",
        );
        assert!(
            decision.contains("None => true"),
            "an unregistered triple must refuse: the tail runs the real bytes \
             and a VM-side body the registry never heard of cannot be audited",
        );
        assert!(
            decision.contains("thin_helper_capability_admits("),
            "a capability-classified triple must ask the capability question \
             before anything else admits it",
        );
        // ...and the per-call answer must be the funnel's own check, not a copy
        // of it: the gate a thin helper stands in for is
        // `check_native_dispatch_capability`, recording included.
        let start = src
            .find("fn thin_helper_capability_admits(")
            .expect("thin_helper_capability_admits must exist");
        let body = &src[start..start + src[start..].find("\n}\n").expect("closed item")];
        assert!(
            body.contains("check_native_dispatch_capability_for_slot("),
            "the per-call capability answer must come from the funnel's own \
             function, or a helper can admit what the funnel would deny",
        );
        assert!(
            decision.contains("jdk_only_enforce_shadow()"),
            "an armed `CRATONVM_ENFORCE_NATIVE_SHADOW` must refuse every \
             guarded helper: the dial is keyed on the RECEIVER class and a \
             gate names the implementing one, so the two can disagree, and the \
             dial's whole purpose is to price a retirement exactly",
        );
    }

    /// Bind time and call time are the SAME decision.
    ///
    /// The page's third bullet asks that a site which will refuse on every
    /// call should not bind at all; the converse matters just as much, and
    /// both reduce to the two halves agreeing. They agree because there is one
    /// function.
    #[test]
    fn the_two_halves_of_the_policy_are_one_predicate() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        for caller in [
            "fn direct_native_bind_admitted(",
            "fn direct_helper_tail_would_refuse(",
        ] {
            let start = src
                .find(caller)
                .unwrap_or_else(|| panic!("{caller} must exist"));
            let after = &src[start..];
            let end = after
                .find("\n}\n")
                .unwrap_or_else(|| panic!("{caller} must be a closed item"));
            assert!(
                after[..end].contains("direct_helper_refusal("),
                "{caller} must route through the shared decision, or the \
                 bind-time and call-time answers can differ and a bound helper \
                 refuses on every call -- or, worse, the reverse",
            );
        }
    }

    /// A `Compatible` VM never consults the registry and never touches either
    /// counter -- the property that keeps `--compatible` unmoved, which is
    /// half the page's acceptance criterion.
    #[test]
    fn compatible_mode_short_circuits_before_the_gate() {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        assert!(!crate::vm::dispatch_policy(&shared).is_jdk_only());
        let before = (
            jit_direct_helper_jdk_only_refusals(&shared),
            jit_direct_helper_jdk_only_admissions(&shared),
        );
        // JIT round 20 wave 14 (lane cjavac): the exact-`HashMap` gate answers
        // the `jdk-hash-collections` row (no registry kind, no counter), so its
        // verdict here is that row's; the counters below must still not move.
        assert_eq!(
            jit_direct_helper_refused(&shared, &HASHMAP_GET_GATE),
            hashmap_gate_under_jdk_model(&shared, &HASHMAP_GET_GATE)
        );
        assert!(!jit_direct_helper_refused(&shared, &MD_UPDATE_BYTE_GATE));
        assert_eq!(
            (
                jit_direct_helper_jdk_only_refusals(&shared),
                jit_direct_helper_jdk_only_admissions(&shared),
            ),
            before,
            "the `Compatible` arm must not reach the counters at all",
        );
    }

    /// The decision table: three registered kinds, "unregistered", and a
    /// capability-classified triple.
    ///
    /// Asserted on [`direct_helper_refusal`] rather than through a strict-mode
    /// `SharedVm`, because a `--jdk-only` VM cannot be constructed in-process
    /// here at all -- `vm_init` refuses `jdk-only` paired with the synthetic
    /// class library, and a real JDK is not a test dependency. That costs
    /// nothing: both halves of the policy reach this function having already
    /// established `JdkOnly` for themselves, and
    /// `the_two_halves_of_the_policy_are_one_predicate` is what pins that they
    /// reach it at all. The mode is the callers' term; the table is this
    /// function's.
    ///
    /// The `Bridge` row is the page. Before 2026-09-22 the answer there was
    /// "refuse", the site fell through to `vm_exec::invoke_or_native`, and
    /// that function ran the same `Bridge` -- so the refusal bought no policy
    /// and cost a three-string registry hash per call.
    #[test]
    fn the_refusal_table_admits_a_bridge_the_tail_would_run_and_nothing_else() {
        use cratonvm_native_api::{CapabilityKind, NativeKind};

        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let (class, method, descriptor) = MD_UPDATE_BYTE_GATE.triple();
        let refuse = |capability, kind| {
            direct_helper_refusal(&shared, class, method, descriptor, capability, kind)
        };

        assert!(
            !refuse(None, Some(NativeKind::Bridge)),
            "a Bridge with no concrete bytecode behind it is what the tail \
             runs, so refusing the helper only buys a slower route to the same \
             native -- the defect this page names",
        );
        assert!(
            !refuse(None, Some(NativeKind::Intrinsic)),
            "the reviewed exception may shadow bytecode",
        );
        assert!(
            refuse(None, Some(NativeKind::SyntheticStub)),
            "a fake may not be invoked under --jdk-only; the tail raises that, \
             so the helper must hand it the call",
        );
        assert!(
            refuse(None, None),
            "an unregistered triple must refuse: the tail runs the real bytes \
             and a VM-side body the registry never heard of cannot be audited",
        );
        // A classified triple asks the capability question -- the funnel's own
        // `check_native_dispatch_capability_for_slot` per call, a side-effect
        // free "could the policy deny it" at bind time -- and under the policy
        // a VM boots with (Permissive) nothing is denied. Until 2026-09-24 this
        // row asserted a blanket refusal, which left every `Unsafe` helper bound
        // nowhere; see `thin_helper_capability_admits`.
        let raw = CapabilityKind::RawMemory;
        assert!(
            !refuse(
                Some((raw, CapabilityQuestion::Bind)),
                Some(NativeKind::Bridge)
            ),
            "Permissive denies nothing, so a classified Bridge binds",
        );
        assert!(
            !refuse(
                Some((raw, CapabilityQuestion::Call(None))),
                Some(NativeKind::Bridge)
            ),
            "and each call passes the funnel's own check",
        );
    }

    /// Under `Enforce` the classified helpers are refused at BOTH halves unless
    /// the policy grants the kind -- the gate the funnel runs, not a weaker
    /// copy of it, and the same fail-closed `Scope::Any` rule
    /// `enforce_admits_a_dispatch_covered_by_an_unscoped_grant` pins there.
    #[test]
    fn a_classified_helper_is_refused_exactly_when_enforce_would_deny_the_funnel() {
        use cratonvm_native_api::{
            install_capabilities, Capability, CapabilityKind, CapabilityMode, CapabilitySet,
            NativeKind, VmId,
        };

        let (class, method, descriptor) = UNSAFE_CAS_INT_GATE.triple();
        let refused = |shared: &SharedVm, q: CapabilityQuestion| {
            direct_helper_refusal(
                shared,
                class,
                method,
                descriptor,
                Some((CapabilityKind::RawMemory, q)),
                Some(NativeKind::Bridge),
            )
        };
        let with_policy = |set: CapabilitySet| {
            let mut shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
            let caps = std::sync::Arc::new(set);
            shared
                .natives
                .native_methods
                .set_capabilities(std::sync::Arc::clone(&caps));
            install_capabilities(caps);
            shared
        };

        let probe = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let vm = VmId::from_raw(probe.vm_identity);
        let denied = with_policy(CapabilitySet::new(vm, CapabilityMode::Enforce));
        assert!(
            refused(&denied, CapabilityQuestion::Bind),
            "Enforce, no grant: no bind"
        );
        assert!(
            refused(&denied, CapabilityQuestion::Call(None)),
            "Enforce, no grant: a bound site declines every call to the funnel, \
             which raises the denial",
        );

        let mut set = CapabilitySet::new(vm, CapabilityMode::Enforce);
        set.grant(Capability::parse_grant("raw-memory:*").expect("grant parses"));
        let granted = with_policy(set);
        assert!(
            !refused(&granted, CapabilityQuestion::Bind),
            "an unscoped grant binds"
        );
        assert!(
            !refused(&granted, CapabilityQuestion::Call(None)),
            "an unscoped grant admits"
        );

        // `Audit` never denies, so it binds; its per-call check still records.
        let audit = with_policy(CapabilitySet::new(vm, CapabilityMode::Audit));
        assert!(!refused(&audit, CapabilityQuestion::Bind));
        assert!(!refused(&audit, CapabilityQuestion::Call(None)));
    }

    /// nio21 wave 2 L1 (edit D of the r21 CHM-row page): the `--compatible`
    /// arm refuses the segmented `ConcurrentHashMap.get` helper exactly when
    /// the `jdk-concurrent-hash-map` row is in force, and no other gate.
    #[test]
    fn chm_gate_refuses_only_under_the_compat_chm_row() {
        use cratonvm_types::HotSpotAlignment;
        let mut shared = SharedVm::new(crate::config::VmConfig::default());
        let regs = &mut shared.natives.native_methods;
        regs.set_real_jdk(true);
        let off = regs
            .hotspot_alignments()
            .with(HotSpotAlignment::JdkConcurrentHashMap, false);
        regs.set_hotspot_alignments(off);
        assert!(!chm_gate_under_jdk_model(
            &shared,
            &CONCURRENT_HASHMAP_GET_GATE
        ));
        let regs = &mut shared.natives.native_methods;
        regs.set_hotspot_alignments(off.with(HotSpotAlignment::JdkConcurrentHashMap, true));
        assert!(chm_gate_under_jdk_model(
            &shared,
            &CONCURRENT_HASHMAP_GET_GATE
        ));
        assert!(!chm_gate_under_jdk_model(&shared, &HASHMAP_GET_GATE));
    }

    /// JIT round 20 wave 14 (lane cjavac): the `--compatible` arm refuses the
    /// exact-`HashMap` get/put helpers exactly when the `jdk-hash-collections`
    /// row is in force over a real JDK, and no other gate; on the synthetic
    /// library (no real JDK) the row is not in force and the helpers stay.
    #[test]
    fn hashmap_gates_refuse_only_under_the_compat_hash_collections_row() {
        use cratonvm_types::HotSpotAlignment;
        let mut shared = SharedVm::new(crate::config::VmConfig::default());
        assert!(!crate::vm::dispatch_policy(&shared).is_jdk_only());
        let regs = &mut shared.natives.native_methods;
        let on = regs
            .hotspot_alignments()
            .with(HotSpotAlignment::JdkHashCollections, true);
        regs.set_hotspot_alignments(on);
        regs.set_real_jdk(false);
        assert!(!hashmap_gate_under_jdk_model(&shared, &HASHMAP_GET_GATE));
        let regs = &mut shared.natives.native_methods;
        regs.set_real_jdk(true);
        assert!(hashmap_gate_under_jdk_model(&shared, &HASHMAP_GET_GATE));
        assert!(hashmap_gate_under_jdk_model(&shared, &HASHMAP_PUT_GATE));
        assert!(!hashmap_gate_under_jdk_model(
            &shared,
            &CONCURRENT_HASHMAP_GET_GATE
        ));
        assert!(jit_direct_helper_refused(&shared, &HASHMAP_GET_GATE));
        let regs = &mut shared.natives.native_methods;
        regs.set_hotspot_alignments(on.with(HotSpotAlignment::JdkHashCollections, false));
        assert!(!hashmap_gate_under_jdk_model(&shared, &HASHMAP_GET_GATE));
        assert!(!hashmap_gate_under_jdk_model(&shared, &HASHMAP_PUT_GATE));
    }
}

#[cfg(test)]
mod direct_bind_source_witness {
    use super::DirectNativeShadow;

    /// Working-tree path of a file under the `vm` crate.
    ///
    /// `CARGO_MANIFEST_DIR` is `<repo>/vm`, so these read the SOURCE, not a
    /// build artifact — `source-witness-tests-read-the-working-tree`. An edit
    /// that has not been rebuilt still fails this test, which is the point.
    fn vm_src(rel: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
    }

    fn read(rel: &str) -> String {
        let path = vm_src(rel);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
    }

    /// Count code (non-comment) occurrences of `crate::jit::helpers::<sym>` for
    /// any enumerated shadow.
    ///
    /// Comment lines are excluded by their trimmed prefix rather than by a
    /// parser: both doors reference these symbols in prose right beside the
    /// code, and a count that included prose would move whenever someone
    /// improved a comment. CR is trimmed too, so a CRLF checkout does not
    /// change the answer — `windows-crlf-breaks-source-witness-tests`.
    fn raw_address_takes(rel: &str) -> Vec<(usize, String)> {
        let text = read(rel);
        let mut hits = Vec::new();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim_end_matches('\r').trim_start();
            if line.starts_with("//") {
                continue;
            }
            for shadow in DirectNativeShadow::ALL {
                let needle = format!("crate::jit::helpers::{}", shadow.helper_symbol());
                if line.contains(&needle) {
                    hits.push((i + 1, shadow.helper_symbol().to_string()));
                }
            }
        }
        hits
    }

    /// Neither direct door names a helper item any more. `H12-1` O1/O2, landed
    /// 2026-09-22.
    ///
    /// The baseline was **7 and 2** — seven raw address-takes in the OSR
    /// ladder, five of them `bridge` rows (`H12-2` 1), and two in the eager
    /// first-call ladder. Its own doc said what to do when the number fell:
    /// *"If it goes DOWN, O1/O2 are landing and the baseline should be lowered
    /// in the same commit; at zero, delete this module and make the nine items
    /// `pub(in crate::jit)`, which is strictly better than any test."*
    ///
    /// Both halves of that happened, and the module is kept rather than
    /// deleted for the half that is NOT covered by the visibility change: the
    /// thirty-one items are `pub(in crate::jit)` now, so a ladder outside this
    /// module cannot name one and the compiler says so — but nothing stops a
    /// future change widening one back to `pub` on its way to adding a
    /// thirty-second door. This test and
    /// [`every_enumerated_helper_is_private_to_the_jit_module`] are the two
    /// halves of that: one says the doors do not name them, the other says
    /// they could not.
    #[test]
    fn neither_direct_door_takes_a_raw_helper_address_any_more() {
        let osr = raw_address_takes("src/runtime/interpreter/jit_bridge.rs");
        let eager = raw_address_takes("src/runtime/interpreter.rs");
        assert!(
            osr.is_empty(),
            "the OSR ladder names a helper item directly: {osr:?}. Every bind \
             at that door goes through `admit_direct_native_entry`, which is \
             the only thing that asks the policy question. Was 7 before \
             `H12-1` O1."
        );
        assert!(
            eager.is_empty(),
            "the eager first-call ladder names a helper item directly: \
             {eager:?}. Was 2 before `H12-1` O2; both of its binds are \
             `intrinsic` and therefore correct, which is exactly why an \
             unguarded ladder there was a defect waiting for its third bind."
        );
    }

    /// Every enumerated helper is `pub(in crate::jit)`.
    ///
    /// This is `H12-1` N1's ask as it actually landed. N1 wanted `JitDirectCall`
    /// made un-constructible without a policy witness; the H20-1 module block
    /// above explains why the token has to ride on the ADDRESS instead (a
    /// direct-call row filtered downstream leaves the pc with no `invoke_info`,
    /// and the backend compiles the site as a call to the enclosing method). The
    /// address is handed out by `admit_direct_native_entry` alone, and this is
    /// what makes "alone" true rather than aspirational.
    ///
    /// A source witness and not a compile check because the compile check is
    /// the *consequence*: if someone widens one of these back to `pub`, nothing
    /// fails to build until a door actually misuses it, and by then the
    /// widening is old.
    #[test]
    fn every_enumerated_helper_is_private_to_the_jit_module() {
        // `vm/src/jit/helpers.rs` is split into `vm/src/jit/helpers/*.rs`
        // (origin/dev `00529e979f`); "this file" is their concatenation.
        let text = crate::jit::helpers::read_helpers_src_from_disk();
        for shadow in DirectNativeShadow::ALL {
            let sym = shadow.helper_symbol();
            let decl = format!("pub(in crate::jit) unsafe extern \"C\" fn {sym}(");
            assert!(
                text.contains(&decl),
                "{shadow:?} names `{sym}`, which is not declared \
                 `pub(in crate::jit) unsafe extern \"C\" fn`. Widening one of \
                 these back to `pub` puts a raw address back within reach of a \
                 compile door, which is the `H12-1` defect: a ladder that binds \
                 a helper without asking whether the callee would serve it."
            );
        }
    }

    /// Every enumerated shadow has a distinct symbol, triple and address.
    ///
    /// A duplicated address would mean two variants naming one helper, which
    /// would make a refusal for one silently refuse the other;
    /// `duplicate-native-registrations-verify-which-wins` is the adjacent trap.
    /// A duplicated TRIPLE would be the H7-1 shape: two helpers asking about
    /// one registry row, one of which is not the row its call actually reaches.
    #[test]
    fn the_enumeration_is_injective() {
        let mut symbols: Vec<&str> = DirectNativeShadow::ALL
            .iter()
            .map(|s| s.helper_symbol())
            .collect();
        symbols.sort_unstable();
        symbols.dedup();
        assert_eq!(symbols.len(), DirectNativeShadow::ALL.len());

        let mut triples: Vec<(&str, &str, &str)> =
            DirectNativeShadow::ALL.iter().map(|s| s.triple()).collect();
        triples.sort_unstable();
        triples.dedup();
        assert_eq!(triples.len(), DirectNativeShadow::ALL.len());

        let mut addrs: Vec<usize> = DirectNativeShadow::ALL
            .iter()
            .map(|s| s.entry_addr())
            .collect();
        assert!(
            addrs.iter().all(|&a| a != 0),
            "a shadow's address is never 0; 0 is the ladders' \"not wired\" sentinel"
        );
        addrs.sort_unstable();
        addrs.dedup();
        assert_eq!(
            addrs.len(),
            DirectNativeShadow::ALL.len(),
            "two variants resolve to one helper address"
        );
    }

    /// The enumerated symbol names are the names the ladders actually write.
    ///
    /// `helper_symbol` is a string, so it can drift from the item it names.
    /// This pins it to the working tree: every symbol must be declared here.
    #[test]
    fn every_enumerated_symbol_is_a_real_item_in_this_file() {
        // `vm/src/jit/helpers.rs` is split into `vm/src/jit/helpers/*.rs`
        // (origin/dev `00529e979f`); "this file" is their concatenation.
        let text = crate::jit::helpers::read_helpers_src_from_disk();
        for shadow in DirectNativeShadow::ALL {
            let decl = format!("extern \"C\" fn {}(", shadow.helper_symbol());
            assert!(
                text.contains(&decl),
                "{:?} names `{}`, which is not declared in this file",
                shadow,
                shadow.helper_symbol()
            );
        }
    }

    /// `DirectNativeShadow::for_scoped_memory_op` agrees with the jit crate's
    /// own recognition of the same sixteen methods.
    ///
    /// Two enums in two crates describe one population — the jit crate cannot
    /// name a VM symbol, which is the whole reason both exist — so the risk is
    /// that two hand-written orderings drift and a `putLong` site binds
    /// `putInt`'s policy answer. Round-tripping through `is_scoped_memory_op`
    /// closes that: the shadow's own triple, fed back to the recogniser, must
    /// produce the op the map was asked about.
    #[test]
    fn every_scoped_memory_op_maps_to_the_shadow_that_spells_its_triple() {
        let mut seen = 0usize;
        for shadow in DirectNativeShadow::ALL {
            let (class, method, descriptor) = shadow.triple();
            if class != "jdk/internal/misc/ScopedMemoryAccess" {
                continue;
            }
            seen += 1;
            let op = cratonvm_jit::is_scoped_memory_op(class, method, descriptor)
                .unwrap_or_else(|| panic!("{shadow:?} spells a triple `is_scoped_memory_op` does not recognise: {class}.{method}{descriptor}"));
            assert_eq!(
                DirectNativeShadow::for_scoped_memory_op(op),
                *shadow,
                "`for_scoped_memory_op({op:?})` does not round-trip to the \
                 shadow whose triple spells that op"
            );
        }
        assert_eq!(
            seen, 18,
            "the `ScopedMemoryAccess` family is eight unaligned accessors and \
             their eight `*Internal` twins plus the two byte internals; found {seen}"
        );
    }
}

#[cfg(test)]
mod varhandle_read_direct_helper_tables {
    use super::*;

    /// Every slot has its OWN monomorphisation.
    ///
    /// The slot is the only thing that tells one of these helpers from another
    /// — it picks the return kind the read is checked against and the synthetic
    /// call site the cold arm dispatches through. Two slots sharing an address
    /// would not fail to compile and would not fail loudly: a `getAcquire`
    /// site returning `J` would run `get`'s `Z` body, read the right field and
    /// return the wrong width.
    #[test]
    fn every_slot_is_its_own_distinct_function() {
        let addrs = varhandle_read_direct_fns();
        // addr-keyed: not a heap address -- test-local helper function addresses.
        let unique: std::collections::HashSet<usize> = addrs.iter().copied().collect();
        assert_eq!(
            unique.len(),
            cratonvm_jit::VARHANDLE_READ_SLOTS,
            "slot monomorphisations collapsed onto {} addresses",
            unique.len(),
        );
        assert!(addrs.iter().all(|a| *a != 0));
        for (slot, addr) in addrs.iter().enumerate() {
            assert_eq!(varhandle_read_direct_fn(slot), *addr, "slot {slot}");
        }
    }

    /// The synthetic call site at a slot describes the SAME (mode, return kind)
    /// pair `cratonvm_jit::varhandle_read_helper_slot` decodes that slot as.
    ///
    /// This is the seam between the two crates: the JIT picks a slot from the
    /// site's name and descriptor, and the cold arm hands the generic
    /// dispatcher this table's entry for that slot. If they disagree, a
    /// declined `getAcquire` read is dispatched as a `get` — the same value on
    /// this VM, but the wrong method name in every exception and census the
    /// dispatch produces, and a real divergence the day the two modes stop
    /// sharing a callback.
    #[test]
    fn each_synthetic_call_site_matches_its_slot() {
        for (slot, info) in VARHANDLE_READ_INFOS.iter().enumerate() {
            let mode = cratonvm_jit::varhandle_read_slot_mode(slot);
            assert_eq!(info.class_name, "java/lang/invoke/VarHandle");
            assert_eq!(info.method_name, cratonvm_jit::VARHANDLE_READ_MODES[mode]);
            assert_eq!(
                info.return_type,
                cratonvm_jit::varhandle_read_slot_return(slot)
            );
            assert_eq!(info.invoke_kind, 0, "a read mode is an invokevirtual site");
            // The receiver (the handle) plus one coordinate.
            assert_eq!(info.num_jit_args, 2);
            // And the round trip. The two REFERENCE slots share one descriptor
            // by construction — `REF_STRICT`'s is erased, which is the whole
            // reason `varhandle_read_direct_impl` reproduces W6-1 for it
            // instead of leaving it to the descriptor — so the round trip is
            // asserted to land on the `REF_OBJECT` slot of the SAME mode
            // rather than on `slot` itself.
            let round_trip =
                cratonvm_jit::varhandle_read_helper_slot(info.method_name, info.descriptor);
            let expected = if cratonvm_jit::varhandle_read_slot_is_reference(slot) {
                mode * cratonvm_jit::VARHANDLE_READ_KINDS
                    + cratonvm_jit::VARHANDLE_READ_KIND_REF_OBJECT
            } else {
                slot
            };
            assert_eq!(round_trip, Some(expected), "slot {slot}");
        }
    }

    /// What each erased descriptor is allowed to be, per kind.
    ///
    /// * a PRIMITIVE slot's substitution is unobservable, because the
    ///   descriptor's only readers — the argument decode,
    ///   `unbox_poly_return_checked`'s return-type rules and
    ///   `coerce_native_return` — all see the identical return char;
    /// * a `REF_OBJECT` slot's is not a substitution at all: a site of that
    ///   kind declares exactly `(L…;)Ljava/lang/Object;`, and W6-1 cannot fire
    ///   at an `Object` return anyway;
    /// * a `REF_STRICT` slot's IS erased, and that is exactly why
    ///   `varhandle_strict_reference_return_check` exists. This row is the one
    ///   that says the erasure is deliberate rather than an oversight — the
    ///   pairing that must not be broken silently is "erased descriptor here,
    ///   W6-1 reproduced there".
    ///
    /// An `[` or `V` return in this table would be a slot the recogniser can
    /// never produce, so it is asserted absent rather than handled.
    #[test]
    fn every_synthetic_call_site_carries_its_kinds_erasure() {
        for (slot, info) in VARHANDLE_READ_INFOS.iter().enumerate() {
            assert!(info.descriptor.starts_with("(Ljava/lang/Object;)"));
            assert!(!matches!(info.return_type, b'[' | b'V'));
            let kind = cratonvm_jit::varhandle_read_slot_kind(slot);
            if kind == cratonvm_jit::VARHANDLE_READ_KIND_REF_OBJECT
                || kind == cratonvm_jit::VARHANDLE_READ_KIND_REF_STRICT
            {
                assert_eq!(info.return_type, b'L', "slot {slot}");
                assert_eq!(info.descriptor, "(Ljava/lang/Object;)Ljava/lang/Object;");
            } else {
                assert!(
                    !matches!(info.return_type, b'L'),
                    "{} returns {}",
                    info.descriptor,
                    info.return_type as char,
                );
            }
        }
        // The pairing, stated as an assertion rather than only in prose: the
        // strict kind is the only one whose descriptor lies, and the file that
        // holds this table also holds the check that makes the lie harmless.
        // A source witness, for the reason `source-witness-tests-read-the-working-tree`
        // gives: `CARGO_MANIFEST_DIR` is `<repo>/vm`, so this reads the working
        // tree and an unbuilt edit still fails here. What it pins is that the
        // erased descriptor above is CALLED-OUT-TO rather than merely declared:
        // deleting the call and keeping the function would leave the strict
        // slot silently unchecked.
        // `vm/src/jit/helpers.rs` is split into `vm/src/jit/helpers/*.rs`;
        // this reads their working-tree concatenation.
        let text = crate::jit::helpers::read_helpers_src_from_disk();
        assert!(
            text.contains("varhandle_strict_reference_return_check(vm, slot, bits)"),
            "REF_STRICT carries an erased descriptor with nothing reproducing W6-1",
        );
    }
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
unsafe fn const_thunk_self_test(
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
const FFM_KIND_MASK: i64 = 7;

/// Split the helpers' `layout | kind` argument into `(layout address, kind)`.
#[inline]
fn ffm_split_layout_kind(layout_kind: i64) -> (u64, i64) {
    // Cast: an object address, compared as a key and never dereferenced here.
    (
        (layout_kind & !FFM_KIND_MASK) as u64,
        layout_kind & FFM_KIND_MASK,
    )
}

/// The alignment mask the native published for `layout` (see
/// `ffm_fast::layout_align_mask`), or `None` to decline.
#[inline]
fn ffm_layout_align_mask(layout: u64) -> Option<u64> {
    cratonvm_native_builtins::ffm_fast::layout_align_mask(layout)
}

/// Byte width of a kind code, or `None` for one this fast path does not decode.
#[inline]
fn ffm_kind_width(kind: i64) -> Option<i64> {
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
const FFM_SLOT_PTR: usize = 0;
/// See [`FFM_SLOT_PTR`].
const FFM_SLOT_SIZE: usize = 1;
/// See [`FFM_SLOT_PTR`].
const FFM_SLOT_OFFSET: usize = 5;
/// The only carrier shape this fast path decodes — matched by the publish gate
/// in `native-builtins`' `ffm_publish_verdict`.
const FFM_CARRIER_SLOTS: u32 = 6;

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
unsafe fn ffm_read_long_slot(obj_ptr: i64, index: usize) -> Option<i64> {
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
unsafe fn ffm_resolve_carrier(seg: i64, want_write: bool) -> Option<(u64, i64)> {
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
fn ffm_checked_offset(index: i64, width: i64, size: i64) -> Option<i64> {
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
struct RealSegEntry {
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
struct RealSegHit {
    facts: cratonvm_native_builtins::lang_invoke::SegRealFacts,
    state_off: usize,
    state_compact: bool,
}

const REAL_SEG_WAYS: usize = 8;

thread_local! {
    static REAL_SEG_ENTRIES: std::cell::RefCell<([Option<RealSegEntry>; REAL_SEG_WAYS], usize)> =
        const { std::cell::RefCell::new(([None; REAL_SEG_WAYS], 0)) };
}

/// The memoised facts for this access, if any (no context, no field read).
#[inline]
fn real_seg_lookup(
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
pub(crate) fn forget_real_seg_entries() {
    let _ = REAL_SEG_ENTRIES.try_with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            guard.0 = [None; REAL_SEG_WAYS];
            guard.1 = 0;
        }
    });
}

fn real_seg_install(entry: RealSegEntry) {
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
enum FfmCoord {
    Index(i64),
    Byte(i64),
}

impl FfmCoord {
    /// The byte offset this coordinate names for an element of `width`
    /// bytes, or `None` on overflow (a decline).
    #[inline]
    fn byte_offset(self, width: i64) -> Option<i64> {
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
const FFM_BYTE_OFFSET_MARK: i64 = cratonvm_jit::FFM_BYTE_OFFSET_MARK;

/// The site carrier (`B S C I J F D`) of an `FFM_KIND_*` code, or `None`.
#[inline]
fn ffm_site_kind(kind: i64) -> Option<u8> {
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
unsafe fn ffm_real_memo_serve(
    vm_ptr: i64,
    seg: i64,
    layout_kind: i64,
    coord: FfmCoord,
    write: Option<i64>,
    out: *mut i64,
) -> Option<i64> {
    if vm_ptr == 0 || seg == 0 || !cratonvm_types::flags().natives.ffm_real_memo_first {
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
unsafe fn ffm_real_segment_access(
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

/// r23 wave 2d, lane L8 (`docs/feature-designs/jdk-only-mode.md`):
/// the heap road of [`ffm_real_segment_access`] -- a real heap segment
/// (`MemorySegment.ofArray`), which [`real_seg_fill`] declines, served through
/// its backing array by
/// `cratonvm_native_builtins::lang_invoke::segment_layout_heap_access` (the
/// JDK's checks in its order, any failure a decline). Writes `*out` for a read.
/// Allocates nothing and reaches no safepoint, so the forwarded references stay
/// current. `false` declines (the site's unchanged dispatch runs).
#[cold]
#[inline(never)]
unsafe fn ffm_heap_segment_access(
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
unsafe fn real_seg_fill(
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
unsafe fn real_seg_serve(
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
            let resolved = if f.handle && cratonvm_types::flags().natives.ffm_arena_jit_resolve {
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
            let resolved = if f.handle && cratonvm_types::flags().natives.ffm_arena_jit_resolve {
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
fn ffm_element_site_kind_index(info: &JitInvokeInfo, index: i64) -> Option<(i64, i64)> {
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
fn ffm_element_word_kind_index(word: u32, index: i64) -> Option<(i64, i64)> {
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
fn ffm_element_word_coord(word: u32, index: i64) -> Option<(i64, FfmCoord)> {
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
/// `docs/feature-designs/jdk-only-mode.md`):
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
unsafe fn ffm_element_judged_site(word: i64) -> (i64, *const JitInvokeInfo, u32) {
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

#[cfg(test)]
mod r23w4_ffm_unaligned_offset_coord_tests {
    use super::*;

    /// r23 wave 4: an index scales by the width (overflow declines), a byte
    /// offset is taken as is, and the marker never collides with a converted
    /// index.
    #[test]
    fn coords_name_the_byte_offset_the_road_checks() {
        assert_eq!(FfmCoord::Index(5).byte_offset(4), Some(20));
        assert_eq!(FfmCoord::Index(i64::MAX).byte_offset(4), None);
        assert_eq!(FfmCoord::Byte(1).byte_offset(4), Some(1));
        assert_eq!(FfmCoord::Byte(-3).byte_offset(8), Some(-3), "negative: the bounds check declines");
        for shift in 1..=3 {
            assert!(i64::MIN >> shift != FFM_BYTE_OFFSET_MARK);
        }
    }

    /// r23 wave 5: the one kind-to-carrier table both door entries share.
    #[test]
    fn site_kinds_map_every_ffm_kind() {
        let all: Vec<Option<u8>> = (0..=7).map(ffm_site_kind).collect();
        assert_eq!(
            all,
            vec![Some(b'B'), Some(b'S'), Some(b'C'), Some(b'I'), Some(b'J'), Some(b'F'), Some(b'D'), None]
        );
    }

    /// An unaligned offset takes a byte coordinate only when the site word
    /// says so; an aligned one keeps its element index; an index site never
    /// takes a byte coordinate.
    #[test]
    fn unaligned_offsets_need_the_site_bit() {
        use cratonvm_jit::{
            FFM_ELEMENT_SITE_BYTE_OFFSET as OFF, FFM_ELEMENT_SITE_UNALIGNED_OK as UN,
            FFM_ELEMENT_SITE_VALID as V,
        };
        let int = FFM_KIND_INT as u32; // Cast: a 0..=6 kind code
        assert_eq!(ffm_element_word_coord(V | OFF | UN | int, 8), Some((FFM_KIND_INT, FfmCoord::Index(2))));
        assert_eq!(ffm_element_word_coord(V | OFF | UN | int, 9), Some((FFM_KIND_INT, FfmCoord::Byte(9))));
        assert_eq!(ffm_element_word_coord(V | OFF | int, 9), None, "switch off at bind: decline");
        assert_eq!(ffm_element_word_coord(V | int, 9), Some((FFM_KIND_INT, FfmCoord::Index(9))));
        assert_eq!(ffm_element_word_coord(OFF | UN | int, 9), None, "not a site");
    }
}

#[cfg(test)]
mod r23w3_ffm_element_site_kind_index_tests {
    use super::*;

    fn row(method_name: &'static str, descriptor: &'static str) -> JitInvokeInfo {
        JitInvokeInfo {
            class_name: "java/lang/foreign/MemorySegment",
            method_name,
            descriptor,
            num_jit_args: 3,
            return_type: b'I',
            invoke_kind: 2,
            declaring_class_id: 0,
            owner_class_id: 0,
        }
    }

    /// r23 w3 review: an index site passes its index through; an offset site
    /// converts a width-aligned byte offset (negative included: the road
    /// declines it) and declines any other offset to the JDK body.
    #[test]
    fn offsets_convert_only_when_aligned() {
        let int_get = "(Ljava/lang/foreign/ValueLayout$OfInt;J)I";
        if cratonvm_jit::ffm_kind_for_descriptor(int_get).is_none() {
            // `CRATONVM_JIT_NO_FFM_INTRINSIC=1` in this environment.
            return;
        }
        let at = row("getAtIndex", int_get);
        assert_eq!(ffm_element_site_kind_index(&at, 5), Some((FFM_KIND_INT, 5)));
        let off = row("get", int_get);
        assert_eq!(ffm_element_site_kind_index(&off, 8), Some((FFM_KIND_INT, 2)));
        assert_eq!(ffm_element_site_kind_index(&off, 6), None);
        assert_eq!(ffm_element_site_kind_index(&off, -4), Some((FFM_KIND_INT, -1)));
        let byte_set = row("set", "(Ljava/lang/foreign/ValueLayout$OfByte;JB)V");
        assert_eq!(ffm_element_site_kind_index(&byte_set, 7), Some((FFM_KIND_BYTE, 7)));
        let long_get = row("get", "(Ljava/lang/foreign/ValueLayout$OfLong;J)J");
        assert_eq!(ffm_element_site_kind_index(&long_get, 12), None);
        assert_eq!(ffm_element_site_kind_index(&long_get, 16), Some((FFM_KIND_LONG, 2)));
    }
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
unsafe fn jit_ffm_segment_get_body(
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
unsafe fn ffm_carrier_get_body(seg: i64, index: i64, layout_kind: i64, out: *mut i64) -> i64 {
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
unsafe fn jit_ffm_segment_set_body(
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
unsafe fn ffm_carrier_set_body(seg: i64, index: i64, layout_kind: i64, value: i64) -> i64 {
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

#[cfg(test)]
mod ffm_layout_kind_tests {
    use super::*;

    /// Round 12 wave 5 (lane ffm2): the call site ORs the kind into the layout
    /// reference; every kind code must fit under the mask and split back out
    /// with the layout address intact.
    #[test]
    fn the_layout_and_the_kind_split_back_apart() {
        let layout: i64 = 0x7f00_1234_5678;
        assert_eq!(layout & FFM_KIND_MASK, 0, "a heap reference is 8-aligned");
        for kind in [
            FFM_KIND_BYTE,
            FFM_KIND_SHORT,
            FFM_KIND_CHAR,
            FFM_KIND_INT,
            FFM_KIND_LONG,
            FFM_KIND_FLOAT,
            FFM_KIND_DOUBLE,
        ] {
            assert!(
                kind & !FFM_KIND_MASK == 0,
                "kind {kind} does not fit the mask"
            );
            assert_eq!(ffm_split_layout_kind(layout | kind), (layout as u64, kind));
        }
        // A null layout (the site passed `null`) still splits, and declines.
        assert_eq!(ffm_split_layout_kind(FFM_KIND_INT), (0, FFM_KIND_INT));
        assert_eq!(ffm_layout_align_mask(0), None);
    }

    /// A layout the native never read in host order has no verdict, so the
    /// fast path declines (the native then swaps or throws).
    #[test]
    fn a_layout_without_a_verdict_is_declined() {
        assert_eq!(ffm_layout_align_mask(0x7f00_1234_5678), None);
    }
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
/// * the `--jdk-only` half: [`direct_helper_tail_would_refuse`] for the row's
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
fn unsafe_cae_admitted_bits(shared: &crate::vm::SharedVm) -> usize {
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
pub(crate) fn unsafe_inline_admitted_word_for_compile(shared: &crate::vm::SharedVm) -> usize {
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
fn unsafe_field_cells_would_bake(shared: &crate::vm::SharedVm) -> bool {
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
fn unsafe_subword_admitted_bits(shared: &crate::vm::SharedVm) -> usize {
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
fn unsafe_qu_admitted_bits(shared: &crate::vm::SharedVm) -> usize {
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
    if dh::unsafe_ref_get_and_set_field_enabled()
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
fn unsafe_ref_array_admitted_bits(shared: &crate::vm::SharedVm) -> usize {
    use cratonvm_jit::unsafe_ref_array_arm as ra;
    let heap = &shared.mem.heap;
    if !ra::unsafe_ref_array_heap_admits(
        heap.is_generational() || heap.is_g1(),
        cratonvm_types::narrow_oop::narrow_oops_enabled(),
        crate::runtime::env_cache::unsafe_ref_array_hw_cas_enabled(),
    ) {
        return 0;
    }
    let mut bits = 0usize;
    if !direct_helper_tail_would_refuse(shared, &UNSAFE_CAS_REFERENCE_GATE) {
        bits |= 1 << ra::UNSAFE_REF_ARRAY_CAS_GATE_BIT;
    }
    if !direct_helper_tail_would_refuse(shared, &UNSAFE_GET_REFERENCE_VOLATILE_GATE) {
        bits |= 1 << ra::UNSAFE_REF_ARRAY_GET_GATE_BIT;
    }
    bits
}

fn unsafe_inline_admitted_mask(shared: &crate::vm::SharedVm) -> usize {
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

#[cfg(test)]
mod unsafe_inline_admitted_mask_tests {
    use super::*;

    /// The VM's gate list and the JIT's bit order name the same triples in the
    /// same order: a swapped pair would admit inline code for one accessor on
    /// the strength of another's policy answer.
    #[test]
    fn the_gate_order_is_the_jit_bit_order() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let start = src
            .find("fn unsafe_inline_admitted_mask(")
            .expect("the mask function exists");
        let body = &src[start..start + src[start..].find("\n}\n").expect("closed item")];
        let names: Vec<&str> = body
            .split('&')
            .skip(1)
            .filter_map(|s| s.split(',').next())
            .map(str::trim)
            .filter(|s| s.starts_with("UNSAFE_") && s.ends_with("_GATE"))
            .collect();
        assert_eq!(
            names,
            [
                "UNSAFE_GET_BYTE_GATE",
                "UNSAFE_GET_SHORT_GATE",
                "UNSAFE_GET_CHAR_GATE",
                "UNSAFE_GET_INT_GATE",
                "UNSAFE_GET_LONG_GATE",
                "UNSAFE_PUT_BYTE_GATE",
                "UNSAFE_PUT_SHORT_GATE",
                "UNSAFE_PUT_CHAR_GATE",
                "UNSAFE_PUT_INT_GATE",
                "UNSAFE_PUT_LONG_GATE",
                "UNSAFE_GET_INT_VOLATILE_GATE",
                "UNSAFE_GET_LONG_VOLATILE_GATE",
                "UNSAFE_PUT_INT_VOLATILE_GATE",
                "UNSAFE_PUT_LONG_VOLATILE_GATE",
                "UNSAFE_CAS_INT_GATE",
                "UNSAFE_CAS_LONG_GATE",
            ],
            "unsafe_inline_admitted_mask's gate list moved; keep it in GATE_BITS order",
        );
        let gates: [&DirectHelperGate; 16] = [
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
        assert_eq!(
            gates.len(),
            cratonvm_jit::direct_helpers::UnsafeInlineAccess::GATE_BITS.len()
        );
        for (i, (method, desc)) in cratonvm_jit::direct_helpers::UnsafeInlineAccess::GATE_BITS
            .iter()
            .enumerate()
        {
            let (c, m, d) = gates[i].triple();
            assert_eq!(
                (c, m, d),
                ("jdk/internal/misc/Unsafe", *method, *desc),
                "bit {i}"
            );
        }
    }

    /// A default `--compatible` VM -- whose installed policy is `Permissive`
    /// (`vm_init`), unless `CRATONVM_CAPABILITY_MODE` says otherwise --
    /// admits every row.
    #[test]
    fn a_default_compatible_vm_admits_every_row() {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        assert!(!crate::vm::dispatch_policy(&shared).is_jdk_only());
        // nio21 wave 4 L4: fourteen rows (the volatile int/long four added);
        // nio21 wave 5 L3: sixteen (the two CAS rows), unless the VM's array
        // CAS is back on its lock.
        let want = if crate::runtime::env_cache::unsafe_array_hw_cas_enabled() {
            0xFFFF
        } else {
            0x3FFF
        };
        assert_eq!(unsafe_inline_admitted_mask(&shared), want);
    }

    /// nio21 wave 5 L3: `CRATONVM_UNSAFE_ARRAY_HW_CAS=0` clears the two CAS
    /// rows (bits 14, 15) and nothing else.
    #[test]
    fn nio21_w5_the_array_cas_kill_switch_clears_only_the_cas_rows() {
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_UNSAFE_ARRAY_HW_CAS", Some("0"))],
            || {
                let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
                unsafe_inline_admitted_mask(&shared)
            },
        );
        assert_eq!(off, 0x3FFF);
    }
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
/// `--jdk-only`, per call, where the strict policy may decline the triple
/// (since nio21 wave 4 L4 the word is published there too when
/// [`current_thread_word_admitted_jdk_only`] proves it cannot), and (b) in any
/// mode, a capability-classified gate.
/// `THREAD_CURRENT_THREAD_GATE` is not classified, and the execution policy
/// is the VM's config (`vm_exec::dispatch_policy`), fixed at creation, so
/// outside `--jdk-only` the helper's fast arm serves every call that finds a
/// thread with a mirror -- which is exactly what the prefix reads, and it
/// declines (to this same helper) for no thread or no mirror. The prefix
/// makes no Rust crossing, so it owes no `note_jit_boundary`, and the
/// helper's `native_pending_return` set/clear pair has no net effect (both
/// happen inside one call). `CRATONVM_JIT_SP_CURRENT_THREAD_PREFIX=0` keeps the
/// word `0`.
fn thread_java_obj_offset_for(shared: &crate::vm::SharedVm) -> usize {
    // `Option<ObjectRef>` is one pointer and `None` is the null word (the
    // `NonNull` niche, `types/src/value.rs`), so the prefix's `TEST; JZ`
    // reads `None` exactly.
    const _: () = assert!(
        std::mem::size_of::<Option<cratonvm_types::ObjectRef>>() == std::mem::size_of::<usize>()
    );
    if !cfg!(target_arch = "x86_64")
        || THREAD_CURRENT_THREAD_GATE.capability.is_some()
        || THREAD_CURRENT_CARRIER_THREAD_GATE.capability.is_some()
        || !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_SP_CURRENT_THREAD_PREFIX")
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
/// the thread word be published under `--jdk-only`?
///
/// The word lets both tiers answer `Thread.currentThread()` AND
/// `Thread.currentCarrierThread()` (the same prefix serves both binds) with no
/// call, i.e. with no per-call gate. Under `--jdk-only` the gate
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
/// (`0` under `--jdk-only`). Compile-time only.
fn current_thread_word_admitted_jdk_only(shared: &crate::vm::SharedVm) -> bool {
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_THREAD_WORD_ALL_MODES")
        || crate::runtime::env_cache::jdk_only_enforce_shadow()
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

pub(crate) fn direct_helper_table_for(
    shared: &crate::vm::SharedVm,
) -> cratonvm_jit::DirectHelperTable {
    let mut table = cratonvm_jit::DirectHelperTable {
        static_base_resolver: jit_resolve_static_base as *const () as usize,
        // Cast: the `SharedVm` address `jit_resolve_static_base` receives back.
        static_base_resolver_ctx: shared as *const crate::vm::SharedVm as usize,
        class_initialized: jit_class_initialized as *const () as usize,
        well_known_class_id: jit_well_known_class_id as *const () as usize,
        ..direct_helper_table()
    };
    // gcg g4/oo hunk 5.2 (b), applied by g4/jm: under the owed-cycle switch,
    // every compiled poll also tests this VM's inline-start latch, so a cycle
    // an allocation deferred (`gen_conc_owed_poll_enabled`) is taken at the
    // next poll by `jit_safepoint_slow_path_body`, never by the allocation
    // helper (`jit_take_gen_inline_concurrent_start` steps aside). Read once
    // per compile; a body compiled before the switch was read keeps its polls.
    // x86-64 only: the aarch64 poll has no second test, so there the latch
    // stays with the allocation helper (see `jit_take_gen_inline_concurrent_start`).
    if cfg!(target_arch = "x86_64")
        && crate::runtime::interpreter::gc_and_alloc::gen_conc_owed_poll_enabled()
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
    // JDK-driven virtual threads (`vm::jdk_continuation`): a carrier's
    // `currentThread()` word names the virtual thread it mounts
    // (`Thread.setCurrentThread`), so `Thread.currentCarrierThread()` is not
    // that word; `0` (no thin bind, no inline prefix in either tier) keeps
    // every site on the native, which answers the carrier.
    if shared.threads.virtual_thread_manager.jdk_continuations() {
        table.thread_current_carrier_thread = 0;
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

/// nio21 wave 8 L2: the thin-helper gates' memos and the bypass-marking latch
/// are per VM (`JitRealm::direct_helper_gates`), not process statics.
#[cfg(test)]
mod nio21_w8_direct_helper_gate_sites_tests {
    use super::*;

    fn jdk_only_vm() -> SharedVm {
        let mut config = crate::config::VmConfig::default();
        config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
        // `JdkOnly` with the synthetic library is a rejected pair.
        config.use_synthetic_jdk = false;
        SharedVm::new(config)
    }

    /// The macro numbers the gates densely, in declaration order, and the
    /// count sizes the per-VM array exactly.
    #[test]
    fn nio21_w8_gate_ordinals_are_dense_and_in_declaration_order() {
        assert_eq!(ALL_DIRECT_HELPER_GATES.len(), DIRECT_HELPER_GATE_COUNT);
        for (i, (name, gate)) in ALL_DIRECT_HELPER_GATES.iter().enumerate() {
            assert_eq!(gate.ordinal, i, "{name}");
        }
    }

    /// VM A in `--jdk-only` and VM B in `--compatible`, in one process: every
    /// gate has a distinct memo per VM, resolving in A leaves B's memo cold,
    /// and resolving in B does not evict A's (the thrash the shared static
    /// memo had: one word, re-filled by whichever VM asked last).
    #[test]
    fn nio21_w8_two_vms_in_two_modes_do_not_share_a_gate_memo() {
        let a = jdk_only_vm();
        let b = SharedVm::new(crate::config::VmConfig::default());
        assert!(crate::vm::dispatch_policy(&a).is_jdk_only());
        assert!(!crate::vm::dispatch_policy(&b).is_jdk_only());
        for (name, gate) in ALL_DIRECT_HELPER_GATES {
            assert!(
                !std::ptr::eq(gate.site(&a), gate.site(&b)),
                "{name}: the two VMs share a memo"
            );
        }
        let ra = &a.natives.native_methods;
        let rb = &b.natives.native_methods;
        for gate in [
            &MD_UPDATE_BYTE_GATE,
            &UNSAFE_CAS_INT_GATE,
            &THREAD_CURRENT_THREAD_GATE,
        ] {
            let (c, m, _) = gate.triple();
            assert!(!gate.site(&a).is_warm(ra), "{c}.{m}: A warm before any ask");
            assert!(!gate.site(&b).is_warm(rb), "{c}.{m}: B warm before any ask");
            let _ = direct_helper_tail_would_refuse(&a, gate);
            assert!(
                gate.site(&a).is_warm(ra),
                "{c}.{m}: A's ask did not fill A's memo"
            );
            assert!(
                !gate.site(&b).is_warm(rb),
                "{c}.{m}: A's ask warmed B's memo"
            );
            let _ = direct_helper_tail_would_refuse(&b, gate);
            assert!(
                gate.site(&b).is_warm(rb),
                "{c}.{m}: B's ask did not fill B's memo"
            );
            assert!(
                gate.site(&a).is_warm(ra),
                "{c}.{m}: B's ask evicted A's memo"
            );
        }
    }

    /// The bypass-marking latch is per VM: marking A does not latch B.
    #[test]
    fn nio21_w8_the_bypass_marking_latch_is_per_vm() {
        let a = jdk_only_vm();
        let b = SharedVm::new(crate::config::VmConfig::default());
        let ga = a.natives.native_methods.generation();
        let gb = b.natives.native_methods.generation();
        assert!(!a.jit.direct_helper_gates.helper_natives_marked_at(ga));
        mark_direct_call_helper_natives_incomplete(&a);
        assert!(a.jit.direct_helper_gates.helper_natives_marked_at(ga));
        assert!(!b.jit.direct_helper_gates.helper_natives_marked_at(gb));
        mark_direct_call_helper_natives_incomplete(&b);
        assert!(b.jit.direct_helper_gates.helper_natives_marked_at(gb));
        assert!(a.jit.direct_helper_gates.helper_natives_marked_at(ga));
    }
}

#[cfg(test)]
mod direct_helper_table_tests {
    use super::*;

    #[test]
    fn every_direct_helper_is_wired_and_the_long_and_integer_helpers_are_distinct() {
        let t = direct_helper_table();
        assert_ne!(
            t.long_value_of, t.integer_value_of,
            "Long.valueOf is wired to Integer.valueOf's helper"
        );
        assert_ne!(
            t.long_long_value, t.integer_int_value,
            "Long.longValue is wired to Integer.intValue's helper"
        );
        assert_ne!(
            t.long_value_of, t.long_long_value,
            "both Long helpers hold one address"
        );
        assert_ne!(t.monitor_enter, t.monitor_exit);
        for (name, addr) in [
            ("integer_value_of", t.integer_value_of),
            ("thread_current_thread", t.thread_current_thread),
            ("thread_local_get", t.thread_local_get),
            ("thread_local_set", t.thread_local_set),
            ("indy_bridge", t.indy_bridge),
            ("monitor_enter", t.monitor_enter),
            (
                "nio_byte_element_served_class",
                t.nio_byte_element_served_class,
            ),
            ("arm_savebase_watch", t.arm_savebase_watch),
            ("disarm_savebase_watch", t.disarm_savebase_watch),
            ("unsafe_get_byte", t.unsafe_get_byte),
            ("unsafe_get_short", t.unsafe_get_short),
            ("unsafe_get_int", t.unsafe_get_int),
            ("unsafe_get_long", t.unsafe_get_long),
            ("unsafe_get_reference", t.unsafe_get_reference),
            (
                "unsafe_get_reference_volatile",
                t.unsafe_get_reference_volatile,
            ),
            ("unsafe_put_byte", t.unsafe_put_byte),
            ("unsafe_put_short", t.unsafe_put_short),
            ("unsafe_put_int", t.unsafe_put_int),
            ("unsafe_put_long", t.unsafe_put_long),
            ("unsafe_cas_int", t.unsafe_cas_int),
            ("unsafe_cas_long", t.unsafe_cas_long),
            // nio21 wave 3, lane L4.
            ("unsafe_get_int_volatile", t.unsafe_get_int_volatile),
            ("unsafe_get_long_volatile", t.unsafe_get_long_volatile),
            ("unsafe_put_int_volatile", t.unsafe_put_int_volatile),
            ("unsafe_put_long_volatile", t.unsafe_put_long_volatile),
            (
                "unsafe_put_reference_volatile",
                t.unsafe_put_reference_volatile,
            ),
            ("unsafe_cas_reference", t.unsafe_cas_reference),
            ("unsafe_full_fence", t.unsafe_full_fence),
            ("reference_refers_to0", t.reference_refers_to0),
            (
                "phantom_reference_refers_to0",
                t.phantom_reference_refers_to0,
            ),
            (
                "thread_current_carrier_thread",
                t.thread_current_carrier_thread,
            ),
            (
                "scoped_memory_get_int_internal",
                t.scoped_memory_get_int_internal,
            ),
            (
                "scoped_memory_put_int_internal",
                t.scoped_memory_put_int_internal,
            ),
        ] {
            assert_ne!(addr, 0, "{name} is not wired");
        }
        assert_eq!(
            (t.static_base_resolver, t.static_base_resolver_ctx),
            (0, 0),
            "a table built without a VM must not carry a resolver"
        );
        assert_eq!(
            t.class_initialized, 0,
            "a table built without a VM must not carry the class-initialised predicate"
        );
        assert_eq!(
            t.well_known_class_id, 0,
            "a table built without a VM must not carry the well-known-class resolver"
        );
        assert!(t
            .varhandle_read
            .iter()
            .chain(&t.varhandle_write)
            .chain(&t.varhandle_cas)
            .all(|&a| a != 0));
    }
}

// ===========================================================================
// r9w3 vmhelpers3 — the negative type-check memo
// (`perf-negative-typechecks-take-the-full-hierarchy-walk-every-time-20260918.md`).
// ===========================================================================
#[cfg(test)]
mod r9w3_vmhelpers3_negative_typecheck_memo {
    use super::*;
    use crate::config::VmConfig;
    use std::sync::Arc;

    fn key(n: u32) -> (usize, usize, usize, u32, bool) {
        (7, 0x1000, 3, n, false)
    }

    /// A memo proven under one epoch is never served under another, and the
    /// probe that notices the move empties it.
    #[test]
    fn a_negative_is_served_only_under_the_epoch_it_was_proven_in() {
        jit_typecheck_negative_cache_put(key(1), 40);
        assert!(jit_typecheck_negative_cached(key(1), 40));
        assert!(
            !jit_typecheck_negative_cached(key(2), 40),
            "a different receiver"
        );
        assert!(
            !jit_typecheck_negative_cached(key(1), 41),
            "a class was defined since: the false must be re-proven"
        );
        assert!(
            !jit_typecheck_negative_cached(key(1), 40),
            "the stale memo was emptied, not merely skipped"
        );
    }

    /// Bounded, least-recently-used eviction; a hit is moved to the front.
    #[test]
    fn the_memo_is_bounded_and_keeps_the_hot_entry() {
        let epoch = 9_000;
        jit_typecheck_negative_cache_put(key(0), epoch);
        for n in 1..JIT_TYPECHECK_NEGATIVE_CACHE_CAP as u32 {
            jit_typecheck_negative_cache_put(key(n), epoch);
            // Touch entry 0 every time so it is never the LRU victim.
            assert!(jit_typecheck_negative_cached(key(0), epoch));
        }
        jit_typecheck_negative_cache_put(key(10_000), epoch);
        assert!(
            jit_typecheck_negative_cached(key(0), epoch),
            "hot entry survived"
        );
        assert!(jit_typecheck_negative_cached(key(10_000), epoch));
        let len = JIT_TYPECHECK_NEGATIVE_CACHE.with(|c| c.borrow().1.len());
        assert_eq!(len, JIT_TYPECHECK_NEGATIVE_CACHE_CAP);
    }

    /// End to end through the entry point compiled code calls: an unrelated
    /// receiver at a RECORDED site answers `0`, and that `false` is memoized;
    /// an annotation-proxy receiver's `false` is not (its answer depends on the
    /// instance, not the class id).
    #[test]
    fn a_recorded_site_false_is_memoized_but_an_annotation_proxy_false_is_not() {
        let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let (target, plain, anno) = {
            let mut cm = shared.classes.class_manager.write();
            let mut mk = |name: &str, fields: usize| {
                cm.try_ensure_synthetic_class(name, fields)
                    .expect("Compatible mode fabricates; this fixture never runs under --jdk-only")
            };
            (
                mk("cratonvm/test/R9w3NegTarget", 0),
                mk("cratonvm/test/R9w3NegPlain", 0),
                // One slot: `annotation_proxy_satisfies_target` reads slot 0
                // (the annotation's type descriptor); null here, so the proxy
                // is an instance of nothing but `Annotation`.
                mk("java/lang/annotation/AnnotationProxy", 1),
            )
        };
        let (name_ptr, name_len) = cratonvm_jit::intern_typecheck_target(
            "cratonvm/test/R9w3NegTarget",
            Some(target.as_u32()),
        );
        let plain_obj = shared.mem.heap.alloc_object(plain, 0);
        let anno_obj = shared.mem.heap.alloc_object(anno, 1);
        let vm_ptr = Arc::as_ptr(&shared) as i64;
        let key_for = |cid: ClassId| {
            (
                shared.vm_identity,
                name_ptr as usize,
                name_len,
                cid.as_u32(),
                false,
            )
        };
        // The epoch is this VM's store slot's (wave 21), which a concurrent
        // test's store may share, and the redefine latch is process-wide;
        // retry until one call runs in a quiet window.
        let mut checked = false;
        for _ in 0..200 {
            if crate::classloading::any_class_redefined() {
                // Sticky, set by some other test: the memo is bypassed by
                // design, so there is nothing to assert about it here.
                return;
            }
            let e0 = typecheck_memo_epoch(&shared);
            // SAFETY: `vm_ptr` names the live VM above; the objects are live
            // heap objects of its heap; the name is an interned `&'static str`.
            let (plain_ans, anno_ans) = unsafe {
                (
                    jit_instanceof(vm_ptr, plain_obj.as_ptr() as i64, name_ptr, name_len as i64),
                    jit_instanceof(vm_ptr, anno_obj.as_ptr() as i64, name_ptr, name_len as i64),
                )
            };
            assert_eq!(plain_ans, 0, "an unrelated class is not an instance");
            assert_eq!(
                anno_ans, 0,
                "an annotation proxy of no type is not an instance"
            );
            if typecheck_memo_epoch(&shared) != e0 {
                continue;
            }
            assert!(
                jit_typecheck_negative_cached(key_for(plain), e0),
                "a recorded-site false for an ordinary class must be memoized"
            );
            assert!(
                !jit_typecheck_negative_cached(key_for(anno), e0),
                "an annotation proxy's false depends on the instance and must not be"
            );
            // And a memoized false still answers false.
            // SAFETY: as above.
            let again = unsafe {
                jit_instanceof(vm_ptr, plain_obj.as_ptr() as i64, name_ptr, name_len as i64)
            };
            assert_eq!(again, 0);
            checked = true;
            break;
        }
        assert!(checked, "no quiet epoch window in 200 tries");
    }
}

// ===========================================================================
// r10 help2 — the POSITIVE type-check/subtype memos had no
// `class_definition_epoch` stamp at all, unlike their negative/target
// siblings above. A `true` proven for a bare `ClassId` number stayed cached
// forever, including across an unload that freed the number and a later
// definition that handed it to an unrelated class.
// ===========================================================================
#[cfg(test)]
mod r10_help2_positive_typecheck_memo_epoch {
    use super::*;

    fn key(n: u32) -> (usize, usize, usize, u32, bool) {
        (7, 0x2000, 3, n, false)
    }

    /// Mirrors `a_negative_is_served_only_under_the_epoch_it_was_proven_in`
    /// for `JIT_TYPECHECK_ANSWER_CACHE`, which used to have no epoch stamp:
    /// a `true` proven for `obj_class_id = n` under one epoch must not answer
    /// for whatever class a later unload hands that same numeric id to.
    #[test]
    fn a_positive_answer_is_served_only_under_the_epoch_it_was_proven_in() {
        jit_typecheck_answer_cache_put(key(1), 40);
        assert!(jit_typecheck_answer_cached(key(1), 40));
        assert!(
            !jit_typecheck_answer_cached(key(2), 40),
            "a different receiver class id"
        );
        assert!(
            !jit_typecheck_answer_cached(key(1), 41),
            "a class was defined or unloaded since: the true must be re-proven, \
             not served for whatever class now holds this ClassId"
        );
        assert!(
            !jit_typecheck_answer_cached(key(1), 40),
            "the stale memo was emptied, not merely skipped"
        );
    }

    /// Bounded, least-recently-used eviction; a hit is moved to the front —
    /// same shape as the negative memo's own eviction test.
    #[test]
    fn the_memo_is_bounded_and_keeps_the_hot_entry() {
        let epoch = 9_500;
        jit_typecheck_answer_cache_put(key(0), epoch);
        for n in 1..JIT_TYPECHECK_ANSWER_CACHE_CAP as u32 {
            jit_typecheck_answer_cache_put(key(n), epoch);
            assert!(jit_typecheck_answer_cached(key(0), epoch));
        }
        jit_typecheck_answer_cache_put(key(10_000), epoch);
        assert!(
            jit_typecheck_answer_cached(key(0), epoch),
            "hot entry survived"
        );
        assert!(jit_typecheck_answer_cached(key(10_000), epoch));
        let len = JIT_TYPECHECK_ANSWER_CACHE.with(|c| c.borrow().1.len());
        assert_eq!(len, JIT_TYPECHECK_ANSWER_CACHE_CAP);
    }

    /// The sibling defect in `JIT_SUBTYPE_POSITIVE_CACHE`, exercised through
    /// `jit_is_subclass_of_cached` with a real (lightweight) `SharedVm`: poison
    /// the table with a `true` for two classes that are NOT actually related,
    /// stamped with an epoch behind the live one -- standing in for "this
    /// entry was proven before the class that used to hold this id was
    /// unloaded and the id was handed to `child`/`parent`" -- and confirm the
    /// stale entry is re-proven rather than served.
    #[test]
    fn a_stale_epoch_positive_subtype_entry_is_reproven_not_served() {
        let shared: std::sync::Arc<SharedVm> =
            std::sync::Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let (child, parent) = {
            let mut cm = shared.classes.class_manager.write();
            let mut mk = |name: &str| {
                cm.try_ensure_synthetic_class(name, 0)
                    .expect("Compatible mode fabricates; this fixture never runs under --jdk-only")
            };
            (
                mk("cratonvm/test/R10Help2Child"),
                mk("cratonvm/test/R10Help2Parent"),
            )
        };
        // Two freshly fabricated classes share no hierarchy beyond `Object`,
        // so the real answer is `false` -- which is exactly what makes a
        // poisoned `true` entry observable as wrong if it were ever served.
        let real_answer = shared
            .classes
            .class_manager
            .read()
            .is_subclass_of(child, parent);
        assert!(
            !real_answer,
            "fixture assumption: two freshly fabricated classes are not subtypes \
             of each other"
        );
        let live_epoch = typecheck_memo_epoch(&shared);
        let stale_epoch = live_epoch.wrapping_sub(1);
        let key = (shared.vm_identity, child.as_u32(), parent.as_u32());
        JIT_SUBTYPE_POSITIVE_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            cache.0 = stale_epoch;
            cache.1.clear();
            cache.1.push(key);
        });
        assert!(
            !jit_is_subclass_of_cached(&shared, child, parent),
            "a positive answer stamped with a stale epoch must be re-proven, not \
             served -- otherwise a ClassId an unload later reused for an unrelated \
             class would inherit someone else's cached `true`"
        );
        let (stamp_after, still_has_poison) = JIT_SUBTYPE_POSITIVE_CACHE.with(|cache| {
            let cache = cache.borrow();
            (cache.0, cache.1.contains(&key))
        });
        assert!(
            stamp_after >= live_epoch,
            "the table must be re-stamped with a current-or-later epoch, not left \
             on the poisoned one"
        );
        assert!(
            !still_has_poison,
            "the poisoned entry must be cleared, not merely skipped and left behind"
        );
    }
}

// ===========================================================================
// Interpreter round i1 wave 21, lane L5 — the typecheck memos and the
// class-identity dispatch flush are stamped with their VM's class-store epoch
// (`interpreter-L2-class-epoch-readers-outside-the-site-caches-stay-process-wide-FIXED-20260925.md`).
// ===========================================================================
#[cfg(test)]
mod w21_l5_per_vm_class_epoch_memos {
    use super::*;
    use crate::config::VmConfig;

    fn define(vm: &SharedVm, name: &str) {
        vm.classes
            .class_manager
            .write()
            .try_ensure_synthetic_class(name, 0)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
    }

    /// Two VMs whose class stores do not share an epoch slot, or `None` when
    /// the domains collide (the documented conservative case).
    fn two_vms() -> Option<(SharedVm, SharedVm)> {
        let mine = SharedVm::new(VmConfig::default());
        let other = SharedVm::new(VmConfig::default());
        (cratonvm_classloading::store_epoch_slot(mine.jit.class_layout_domain)
            != cratonvm_classloading::store_epoch_slot(other.jit.class_layout_domain))
        .then_some((mine, other))
    }

    /// A typecheck answer proven in one VM survives a class definition in
    /// another VM and is retired by one in its own.
    #[test]
    fn a_typecheck_answer_is_retired_only_by_its_own_vms_definitions() {
        let Some((mine, other)) = two_vms() else {
            return;
        };
        let key = (mine.vm_identity, 0x21_0050, 5, 77, false);
        let mut checked = false;
        for round in 0..50 {
            let epoch = typecheck_memo_epoch(&mine);
            jit_typecheck_answer_cache_put(key, epoch);
            define(&other, &format!("cratonvm/test/W21L5TcOther{round}"));
            let now = typecheck_memo_epoch(&mine);
            if now != epoch {
                // A concurrent test's store shares this VM's slot.
                continue;
            }
            assert!(
                jit_typecheck_answer_cached(key, now),
                "another VM's class definition must not retire this VM's answer"
            );
            checked = true;
            break;
        }
        assert!(checked, "no quiet window for this VM's epoch slot");
        let before = typecheck_memo_epoch(&mine);
        define(&mine, "cratonvm/test/W21L5TcMine");
        let after = typecheck_memo_epoch(&mine);
        assert_ne!(after, before, "a definition in this VM moves its epoch");
        assert!(!jit_typecheck_answer_cached(key, after));
    }

    /// The stamp is tagged with the store's slot, so it never equals a
    /// process-wide snapshot or another slot's stamp.
    #[test]
    fn a_typecheck_stamp_names_its_slot() {
        let Some((mine, other)) = two_vms() else {
            return;
        };
        assert_ne!(typecheck_memo_epoch(&mine), typecheck_memo_epoch(&other));
        assert_ne!(
            typecheck_memo_epoch(&mine),
            crate::classloading::class_definition_epoch()
        );
    }

    /// The class-identity flush empties this thread's name-keyed dispatch
    /// memos when ITS VM defines a class, not when another VM does.
    #[test]
    fn the_class_identity_flush_follows_its_own_vms_definitions() {
        let Some((mine, other)) = two_vms() else {
            return;
        };
        let key = jit_site_key(mine.vm_identity, 0x21_0051);
        let present = || WRAPPED_STATIC_CALLEE.with(|c| c.borrow().contains_key(&key));
        let quiet = || {
            (
                cratonvm_classloading::store_epochs(mine.jit.class_layout_domain)
                    .class_definition_epoch(),
                crate::classloading::any_class_redefined(),
            )
        };
        let mut checked = false;
        for round in 0..50 {
            let before = quiet();
            flush_class_identity_dispatch_memos(&mine);
            WRAPPED_STATIC_CALLEE.with(|c| {
                c.borrow_mut().insert(key, ());
            });
            define(&other, &format!("cratonvm/test/W21L5FlOther{round}"));
            flush_class_identity_dispatch_memos(&mine);
            if quiet() != before {
                continue;
            }
            assert!(
                present(),
                "another VM's class definition must not flush this VM's memos"
            );
            checked = true;
            break;
        }
        assert!(checked, "no quiet window for this VM's epoch slot");
        define(&mine, "cratonvm/test/W21L5FlMine");
        flush_class_identity_dispatch_memos(&mine);
        assert!(!present(), "this VM's own definition flushes");
    }
}

// ===========================================================================
// r9w3 vmhelpers3 — the compiled-`ldc` slot memo
// (`perf-compiled-ldc-string-is-a-helper-call-per-execution-20260918.md`).
// ===========================================================================
#[cfg(test)]
mod r9w3_vmhelpers3_ldc_slot_memo {
    use super::*;
    use crate::config::VmConfig;
    use std::sync::Arc;

    /// The redefinition count a fill read before its record.
    fn redefinitions() -> u64 {
        crate::classloading::class_redefinition_count()
    }

    /// The memo answers through a slot the collector rewrites: after a
    /// simulated relocation (the exact call every moving collection makes on
    /// the JNI global-ref table) the SAME memo entry answers the NEW address.
    /// A different VM identity, holder or index never matches.
    #[test]
    fn a_memoized_literal_follows_its_object_across_a_relocation() {
        if ldc_slot_memo_latched_off() {
            // Only under the kill switch, and sticky: bypassed by design.
            return;
        }
        let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let cid = {
            let mut cm = shared.classes.class_manager.write();
            cm.try_ensure_synthetic_class("cratonvm/test/R9w3LdcHolder", 0)
                .expect("Compatible mode fabricates; this fixture never runs under --jdk-only")
        };
        let before = shared.mem.heap.alloc_object(cid, 0);
        let after = shared.mem.heap.alloc_object(cid, 0);
        let holder = ClassId::new(0x00AB_CDEF);
        assert_eq!(ldc_slot_memo_probe(shared.vm_identity, holder, 7), None);

        ldc_slot_memo_fill(&shared, holder, 7, before, redefinitions());
        assert_eq!(
            ldc_slot_memo_probe(shared.vm_identity, holder, 7),
            Some(before.as_ptr() as i64)
        );
        assert_eq!(ldc_slot_memo_probe(shared.vm_identity, holder, 8), None);
        assert_eq!(ldc_slot_memo_probe(shared.vm_identity + 1, holder, 7), None);

        let mut moved = cratonvm_types::PointerMap::new();
        moved.insert(before.as_ptr() as usize, after.as_ptr() as usize);
        shared
            .natives
            .jni_global_refs
            .lock()
            .update_after_gc(&moved);
        assert_eq!(
            ldc_slot_memo_probe(shared.vm_identity, holder, 7),
            Some(after.as_ptr() as i64),
            "the memo must read the collector-maintained slot, not a copied pointer"
        );

        // A second fill of the same site reuses the one global ref.
        let refs = shared.natives.jni_global_refs.lock().count();
        ldc_slot_memo_fill(&shared, holder, 7, after, redefinitions());
        assert_eq!(shared.natives.jni_global_refs.lock().count(), refs);
    }

    /// Round 9 wave 7 (`ldc7`): the compile-time slot for a RECORDED site is
    /// the word the compiled probe reads. It holds the recorded constant, it
    /// follows a relocation in place, the helper memo shares it, and an
    /// unrecorded site gets none (the helper call stays).
    #[test]
    fn the_compile_time_ldc_slot_is_the_collector_maintained_word() {
        if ldc_slot_memo_latched_off() || !compiled_ldc_const_cache_enabled() {
            // Latched (kill switch) / switched off by the environment: `0` by
            // design.
            return;
        }
        let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let cid = {
            let mut cm = shared.classes.class_manager.write();
            cm.try_ensure_synthetic_class("cratonvm/test/R9w7LdcSlotHolder", 0)
                .expect("Compatible mode fabricates; this fixture never runs under --jdk-only")
        };
        let before = shared.mem.heap.alloc_object(cid, 0);
        let after = shared.mem.heap.alloc_object(cid, 0);

        assert_eq!(
            compiled_ldc_slot_for(&shared, cid, 11),
            0,
            "nothing recorded: no slot, the site keeps its helper call"
        );

        crate::runtime::interpreter::constants::store_recorded_cp_constant(
            &shared,
            cid,
            11,
            Value::Object(Some(before)),
        );
        let slot = compiled_ldc_slot_for(&shared, cid, 11);
        assert_ne!(slot, 0, "a recorded site gets a slot");
        // SAFETY: `slot` is a live `Box<ObjectRef>` owned by `shared`'s
        // global-ref table, which outlives this test body.
        let read = || unsafe { *(slot as *const ObjectRef) };
        assert_eq!(read().as_ptr(), before.as_ptr());
        assert_eq!(
            compiled_ldc_slot_for(&shared, cid, 11),
            slot,
            "one slot per site"
        );

        // The helper memo answers through the SAME word.
        ldc_slot_memo_fill(&shared, cid, 11, before, redefinitions());
        assert_eq!(
            ldc_slot_memo_probe(shared.vm_identity, cid, 11),
            Some(before.as_ptr() as i64)
        );

        let mut moved = cratonvm_types::PointerMap::new();
        moved.insert(before.as_ptr() as usize, after.as_ptr() as usize);
        shared
            .natives
            .jni_global_refs
            .lock()
            .update_after_gc(&moved);
        assert_eq!(
            read().as_ptr(),
            after.as_ptr(),
            "the baked address must name the relocated object after a moving collection"
        );
        assert_eq!(
            ldc_slot_memo_probe(shared.vm_identity, cid, 11),
            Some(after.as_ptr() as i64)
        );
    }

    /// Interpreter round i1 wave 18, lane L4: a redefinition retires exactly
    /// the redefined class's slots, and a fill whose read of the record began
    /// before a redefinition is neither published nor memoized. Another
    /// class's slot keeps serving. The process count is not moved here (other
    /// tests memoize in this process); the stamps are set around it instead.
    #[test]
    fn a_redefinition_retires_only_the_redefined_classs_ldc_slots() {
        if ldc_slot_memo_latched_off() || !LDC_SLOT_MEMO_SURVIVES_REDEFINITION {
            return;
        }
        let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let cid = {
            let mut cm = shared.classes.class_manager.write();
            cm.try_ensure_synthetic_class("cratonvm/test/I18L4LdcHolder", 0)
                .expect("Compatible mode fabricates; this fixture never runs under --jdk-only")
        };
        let literal = shared.mem.heap.alloc_object(cid, 0);
        let redefined = ClassId::new(0x00AB_1801);
        let untouched = ClassId::new(0x00AB_1802);
        let slot_of = |holder: ClassId| {
            ldc_global_slots(&shared)
                .lock()
                .get(&(holder.as_u32(), 3))
                .copied()
        };

        // A read that began before a redefinition: nothing published or kept.
        let refs = shared.natives.jni_global_refs.lock().count();
        ldc_slot_memo_fill(
            &shared,
            redefined,
            3,
            literal,
            redefinitions().wrapping_sub(1),
        );
        assert_eq!(slot_of(redefined), None);
        assert_eq!(ldc_slot_memo_probe(shared.vm_identity, redefined, 3), None);
        assert_eq!(
            shared.natives.jni_global_refs.lock().count(),
            refs,
            "a refused publication releases the global ref it made"
        );

        let now = redefinitions();
        ldc_slot_memo_fill(&shared, redefined, 3, literal, now);
        ldc_slot_memo_fill(&shared, untouched, 3, literal, now);
        if redefinitions() != now {
            return; // another test redefined a class meanwhile
        }
        assert!(slot_of(redefined).is_some() && slot_of(untouched).is_some());

        retire_ldc_slots_of_class(&shared, redefined);
        assert_eq!(
            slot_of(redefined),
            None,
            "the redefined class's slot is retired"
        );
        assert!(slot_of(untouched).is_some(), "another class's slot is kept");
        assert_eq!(
            ldc_slot_memo_probe(shared.vm_identity, untouched, 3),
            Some(literal.as_ptr() as i64),
            "and still answers"
        );
    }
}

// ===========================================================================
// r9w3 vmhelpers3 — `jit_post_tlab_init` skips an empty init recipe
// (`perf-zgc-compiled-new-always-takes-the-rust-helper-20260918.md`).
// ===========================================================================
#[cfg(test)]
mod r9w3_vmhelpers3_post_alloc_init_skip {
    use super::*;
    use crate::config::VmConfig;
    use crate::jit::alloc_class_cache::{alloc_class_cache_enabled, ClassAllocInfo, PrimKind};
    use std::sync::Arc;

    #[test]
    fn only_a_published_empty_recipe_skips_the_call() {
        if !alloc_class_cache_enabled() {
            return;
        }
        let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let (plain, prim, fin, unseen) = (
            ClassId::new(0x0007_0001),
            ClassId::new(0x0007_0002),
            ClassId::new(0x0007_0003),
            ClassId::new(0x0007_0004),
        );
        let cache = &shared.jit.jit_alloc_class_cache;
        cache.insert(
            plain.as_u32(),
            ClassAllocInfo {
                has_finalizer: false,
                reference_kind: 0,
                prim_inits: Vec::new().into_boxed_slice(),
                ref_inits: Box::new([]),
            },
        );
        cache.insert(
            prim.as_u32(),
            ClassAllocInfo {
                has_finalizer: false,
                reference_kind: 0,
                prim_inits: vec![(0u32, PrimKind::Int), (1u32, PrimKind::Long)].into_boxed_slice(),
                ref_inits: Box::new([]),
            },
        );
        cache.insert(
            fin.as_u32(),
            ClassAllocInfo {
                has_finalizer: true,
                reference_kind: 0,
                prim_inits: Vec::new().into_boxed_slice(),
                ref_inits: Box::new([]),
            },
        );
        // Round i1 wave 5: int-family defaults are the zeroed body's own
        // reading, so an int-only class owes the helper nothing either.
        let int_only = ClassId::new(0x0007_0005);
        cache.insert(
            int_only.as_u32(),
            ClassAllocInfo {
                has_finalizer: false,
                reference_kind: 0,
                prim_inits: vec![(0u32, PrimKind::Int), (1u32, PrimKind::Int)].into_boxed_slice(),
                ref_inits: Box::new([]),
            },
        );
        assert!(jit_post_alloc_init_is_noop(&shared, plain));
        assert!(
            jit_post_alloc_init_is_noop(&shared, int_only),
            "an int-family default needs no store"
        );
        assert!(
            !jit_post_alloc_init_is_noop(&shared, prim),
            "a long default is owed"
        );
        assert!(
            !jit_post_alloc_init_is_noop(&shared, fin),
            "a finalizer must be registered"
        );
        assert!(
            !jit_post_alloc_init_is_noop(&shared, unseen),
            "no recipe yet: the call is what builds it"
        );
    }

    /// The cached arm stores no int-family default, and the slot still reads
    /// `Int(0)` from the allocator's zeroed body; tagged kinds are still
    /// written.
    #[test]
    fn the_cached_arm_leaves_int_defaults_to_the_zeroed_body() {
        if !alloc_class_cache_enabled() {
            return;
        }
        let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let cid = ClassId::new(0x0007_0011);
        shared.jit.jit_alloc_class_cache.insert(
            cid.as_u32(),
            ClassAllocInfo {
                has_finalizer: false,
                reference_kind: 0,
                prim_inits: vec![
                    (0u32, PrimKind::Int),
                    (1u32, PrimKind::Long),
                    (2u32, PrimKind::Double),
                ]
                .into_boxed_slice(),
                ref_inits: Box::new([]),
            },
        );
        let obj = shared.mem.heap.alloc_object(cid, 3);
        jit_post_alloc_init(&shared, obj, cid);
        assert_eq!(shared.mem.heap.get_field(obj, 0), Value::Int(0));
        assert_eq!(shared.mem.heap.get_field(obj, 1), Value::Long(0));
        assert_eq!(shared.mem.heap.get_field(obj, 2), Value::Double(0.0));
    }

    /// The announce helper's degenerate inputs return the object untouched and
    /// never reach the heap (a null VM, a null object, a zero footprint).
    #[test]
    fn the_zgc_announce_helper_passes_degenerate_inputs_through() {
        // SAFETY: every call below takes an early return before any
        // dereference of `vm_ptr` or `obj_ptr`.
        unsafe {
            assert_eq!(jit_zgc_note_tlab_object(0, 0x1000, 32), 0x1000);
            assert_eq!(jit_zgc_note_tlab_object(0x10, 0, 32), 0);
            assert_eq!(jit_zgc_note_tlab_object(0x10, 0x1000, 0), 0x1000);
        }
    }

    /// Round 9 wave 8 (review8c F1): a trap that reaches the helper sink --
    /// a callee trapped under a COMPILED caller -- applies the per-bci de-spec
    /// rule too. Before the fix only `deopt_resume.rs`'s sink recorded
    /// `(method, bci)`, so `TryLoop.sumThrow`'s failing loop-header BCE guard
    /// was re-emitted on every recompile until the method was retired.
    #[test]
    fn trapped_callee_sink_despecs_a_bci_after_the_per_bci_limit() {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let key = "DespecTrappedCallee.sumThrow:([II)J";
        for n in 1..=4u32 {
            despeculate_trapped_method(
                &shared,
                "DespecTrappedCallee",
                "sumThrow",
                "([II)J",
                cratonvm_types::ClassId::new(0),
                cratonvm_jit::deopt::DeoptReason::BoundsCheck,
                5,
            );
            let despec_now = shared.jit.despec_registry.contains(key, 5);
            if n < 4 {
                assert!(
                    !despec_now,
                    "must NOT de-spec before the per-bci limit (n={n})"
                );
            } else {
                assert!(despec_now, "must de-spec once the per-bci limit is reached");
            }
        }
        let bounds = cratonvm_jit::deopt::DeoptReason::BoundsCheck;
        let spec = cratonvm_jit::deopt::speculation_id(5, bounds);
        assert!(
            shared
                .jit
                .despec_registry
                .contains_speculation(key, 5, spec),
            "the recorded speculation is the BoundsCheck one at bci 5"
        );
        // Per-site: another bci of the same method is untouched.
        assert!(!shared.jit.despec_registry.contains(key, 9));
        // The superseded-artifact sentinel never de-specs.
        for _ in 0..6 {
            despeculate_trapped_method(
                &shared,
                "DespecTrappedCallee",
                "other",
                "()V",
                cratonvm_types::ClassId::new(0),
                cratonvm_jit::deopt::DeoptReason::BoundsCheck,
                u32::MAX,
            );
        }
        let other_key = "DespecTrappedCallee.other:()V";
        assert!(!shared.jit.despec_registry.contains(other_key, u32::MAX));
    }

    /// JIT round 19 wave 12, lane tail12
    /// (`r19-trapped-callee-sink-never-withdraws-a-chain-trapping-splice-20261007`):
    /// an inlined CHAIN trapping through the helper / first-call sinks records
    /// the WILDCARD at the call site once the per-bci limit is reached (the
    /// planner withdraws a splice for that id only); a single frame keeps its
    /// named id; the kill word restores the named record for a chain.
    #[test]
    fn a_chain_trap_at_the_helper_sink_records_the_call_site_wildcard() {
        use cratonvm_jit::deopt::{DeoptReason, ReconstructedFrame, SPECULATION_ID_ANY};
        let chain = ReconstructedFrame {
            method_key: "Callee.elem:(I)I".to_string(),
            bci: 2,
            caller_frames: vec![ReconstructedFrame {
                method_key: "ChainSite.kernel:(I)I".to_string(),
                bci: 7,
                ..ReconstructedFrame::default()
            }],
            ..ReconstructedFrame::default()
        };
        let run = |kill: bool| {
            let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
            let key = "CRATONVM_DEOPT_HELPER_CHAIN_SITE_WILDCARD";
            let word = kill.then_some("0");
            cratonvm_types::flags::with_thread_overrides(&[(key, word)], || {
                for _ in 0..4 {
                    despeculate_trapped_method_of_frame(
                        &shared,
                        "ChainSite",
                        "kernel",
                        "(I)I",
                        cratonvm_types::ClassId::new(0),
                        DeoptReason::BoundsCheck,
                        7,
                        &chain,
                    );
                }
            });
            let k = "ChainSite.kernel:(I)I";
            let any = shared
                .jit
                .despec_registry
                .contains_speculation(k, 7, SPECULATION_ID_ANY);
            let named = shared.jit.despec_registry.contains_speculation(
                k,
                7,
                cratonvm_jit::deopt::speculation_id(7, DeoptReason::BoundsCheck),
            );
            (shared.jit.despec_registry.contains(k, 7), any, named)
        };
        // Switch on (default): the wildcard. `contains_speculation` answers a
        // wildcard entry for any id, so `named` is true too.
        let (despec, any, _) = run(false);
        assert!(despec && any, "the call site is withdrawn by the wildcard");
        // Kill word: the old named record, which the planner does not read.
        let (despec, any, named) = run(true);
        assert!(despec && named);
        assert!(!any, "kill word: no wildcard at the call site");
    }
}

#[cfg(test)]
mod r9w10_exc10_division_guard_retier {
    use super::*;

    /// Only the optimizing tier's zero-divisor guard exit is counted: the
    /// plain resume point AT one of the four integer divisions. Any other
    /// reason, any other opcode, or an unreadable bci is left alone.
    #[test]
    fn only_a_transfer_at_an_integer_division_is_a_division_guard_trap() {
        use cratonvm_jit::deopt::DeoptReason as R;
        for op in [0x6c_u8, 0x6d, 0x70, 0x71] {
            assert!(
                is_division_guard_trap(R::TransferToInterpreter, Some(op)),
                "{op:#x}"
            );
        }
        // fdiv, ddiv, frem, drem never trap; iadd is not a division.
        for op in [0x6e_u8, 0x6f, 0x72, 0x73, 0x60] {
            assert!(
                !is_division_guard_trap(R::TransferToInterpreter, Some(op)),
                "{op:#x}"
            );
        }
        assert!(!is_division_guard_trap(R::TransferToInterpreter, None));
        for reason in [
            R::BoundsCheck,
            R::UncommonTrap,
            R::SpeculationFailed,
            R::PendingException,
        ] {
            assert!(!is_division_guard_trap(reason, Some(0x6c)), "{reason:?}");
        }
    }

    /// The decision fires on the exit that reaches the threshold; later exits
    /// (a long-running caller's baked CALL) re-fire it only at powers of two
    /// (round 21, REVVM221-1), other sites count on their own, and a full
    /// table is cleared rather than grown.
    #[test]
    fn division_guard_counter_fires_once_at_the_threshold() {
        let mut counts: rustc_hash::FxHashMap<u32, u32> = rustc_hash::FxHashMap::default();
        for i in 1..4 {
            assert!(!note_division_guard_trap(&mut counts, 7, 4, 16), "exit {i}");
        }
        assert!(
            note_division_guard_trap(&mut counts, 7, 4, 16),
            "the 4th decides"
        );
        for i in 5..=14 {
            assert_eq!(
                note_division_guard_trap(&mut counts, 7, 4, 16),
                i == 8,
                "exit {i}: only a power of two re-fires"
            );
        }
        assert!(!note_division_guard_trap(&mut counts, 8, 4, 16));

        let mut small: rustc_hash::FxHashMap<u32, u32> = rustc_hash::FxHashMap::default();
        let _ = note_division_guard_trap(&mut small, 1, 4, 2);
        let _ = note_division_guard_trap(&mut small, 2, 4, 2);
        let _ = note_division_guard_trap(&mut small, 3, 4, 2);
        assert_eq!(small.len(), 1, "a full table is cleared before a new key");
    }

    /// JIT round 19 wave 14 (TAIL12-1): the sinks' entry counts nothing with
    /// its switch off, and re-tiers at the threshold with it on.
    #[test]
    fn r19_the_sink_entry_re_tiers_a_hot_division_only_under_its_switch() {
        const C: &str = "R19w14Tail12SinkDivision";
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let id = cratonvm_types::ClassId::new(0);
        let h = cratonvm_jit::ir_method_memo_hash(C, "div", "(II)I");
        let exits = || {
            for _ in 0..DIVISION_GUARD_RETIER_THRESHOLD {
                retier_division_guard_trap_at_sink(
                    &shared,
                    C,
                    "div",
                    "(II)I",
                    id,
                    cratonvm_jit::deopt::DeoptReason::TransferToInterpreter,
                    Some(0x6c),
                    4,
                );
            }
        };
        let key: DivisionGuardSiteKey = (
            shared.vm_identity,
            h,
            0,
            shared.jit.jit_cache.redefine_epoch(),
            4,
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_DIV_GUARD_RETIER_SINKS", None)],
            exits,
        );
        assert!(
            DIVISION_GUARD_TRAPS.with(|t| t.borrow().get(&key).is_none()),
            "off: never counted"
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_DIV_GUARD_RETIER_SINKS", Some("1"))],
            exits,
        );
        assert!(
            !crate::runtime::interpreter::jit_bridge::jit_verdicts(&shared)
                .claim_site_trap_decision(h, id),
            "on: the threshold exit claimed the re-tier"
        );
    }

    /// A non-division trap never touches the counter or the tier memo, so the
    /// existing per-bci de-spec policy is the only one that applies to it.
    #[test]
    fn a_non_division_trap_is_not_retiered() {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        for _ in 0..(DIVISION_GUARD_RETIER_THRESHOLD * 2) {
            retier_hot_division_guard_trap(
                &shared,
                "R9w10Exc10NotADivision",
                "f",
                "(II)I",
                cratonvm_types::ClassId::new(0),
                cratonvm_jit::deopt::DeoptReason::TransferToInterpreter,
                Some(0x60),
                2,
            );
        }
        let h = cratonvm_jit::ir_method_memo_hash("R9w10Exc10NotADivision", "f", "(II)I");
        let key: DivisionGuardSiteKey = (
            shared.vm_identity,
            h,
            0,
            shared.jit.jit_cache.redefine_epoch(),
            2,
        );
        assert!(
            DIVISION_GUARD_TRAPS.with(|t| t.borrow().get(&key).is_none()),
            "an iadd bci is never counted"
        );
    }

    /// Interpreter round i1 wave 23, lane L5
    /// (`i22-L5-ir-site-trap-decisions-are-process-wide-by-method-name`): two
    /// VMs running the same method each re-tier their own copy, and a
    /// redefined version re-tiers again. Until then the decision was a
    /// process static keyed by the method's names: the second VM's claim
    /// failed and its optimizing body stayed installed, exiting on every zero
    /// divisor for the life of the process.
    #[test]
    fn each_vm_and_each_class_version_re_tiers_its_own_hot_division_site() {
        const C: &str = "L5W23PerVmDivisionRetier";
        let a = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let b = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let id = cratonvm_types::ClassId::new(0);
        let h = cratonvm_jit::ir_method_memo_hash(C, "div", "(II)I");
        let exits = |vm: &crate::vm::SharedVm| {
            for _ in 0..DIVISION_GUARD_RETIER_THRESHOLD {
                retier_hot_division_guard_trap(
                    vm,
                    C,
                    "div",
                    "(II)I",
                    id,
                    cratonvm_jit::deopt::DeoptReason::TransferToInterpreter,
                    Some(0x6c),
                    2,
                );
            }
        };
        let decided = |vm: &crate::vm::SharedVm| {
            // A claim that SUCCEEDS here means the re-tier did not take it.
            !crate::runtime::interpreter::jit_bridge::jit_verdicts(vm)
                .claim_site_trap_decision(h, id)
        };
        exits(&a);
        assert!(decided(&a), "VM A re-tiers its copy");
        exits(&b);
        assert!(
            decided(&b),
            "VM B re-tiers its own copy: VM A's decision is not VM B's"
        );
        a.jit
            .note_class_redefinition_of(cratonvm_types::ClassId::new(0x2317), C, false);
        exits(&a);
        assert!(decided(&a), "the redefined version re-tiers again");
    }

    fn local_handler_site(
        candidates: Vec<(&'static str, u32)>,
        declaring_class_id: ClassId,
    ) -> cratonvm_jit::JitLocalHandlerSite {
        cratonvm_jit::JitLocalHandlerSite {
            candidates,
            declaring_class_id: declaring_class_id.as_u32(),
            throw_bci: 0,
            cache: std::sync::atomic::AtomicU64::new(
                cratonvm_jit::JitLocalHandlerSite::CACHE_EMPTY,
            ),
            implicit_cache: cratonvm_jit::JitLocalHandlerSite::empty_implicit_cache(),
            no_collect: false,
        }
    }

    /// i1-L5 catch-type page, wave 4: a candidate whose catch type will not
    /// load DECLINES (`-1`, propagate to the interpreter-side route, which
    /// throws the resolution error) instead of being skipped. Before, the
    /// catch-all behind it was entered with the original exception.
    #[test]
    fn local_handler_lookup_declines_on_an_unloadable_catch_type() {
        const EXC: &str = "cratonvm/test/LocalHandlerSlowExc";
        const MISSING: &str = "cratonvm/test/LocalHandlerSlowMissingCatchType";
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let exc = shared
            .classes
            .class_manager
            .write()
            .try_ensure_synthetic_class(EXC, 0)
            .expect("Compatible mode fabricates");

        let site = local_handler_site(vec![(MISSING, 10), ("", 20)], exc);
        assert_eq!(local_handler_index_slow(&shared, &site, exc), -1);

        // Rows before the failing one are still answered: a catch-all first,
        // or an exact catch type first, takes it without reaching the row.
        let site = local_handler_site(vec![("", 10), (MISSING, 20)], exc);
        assert_eq!(local_handler_index_slow(&shared, &site, exc), 0);
        let site = local_handler_site(vec![(EXC, 10), (MISSING, 20)], exc);
        assert_eq!(local_handler_index_slow(&shared, &site, exc), 0);
    }

    /// Wave 6: a declaring class with its own loader namespace never resolves
    /// an unloaded catch type through the loader-blind `load_class_concurrent`
    /// (which could pick a same-named class-path copy and skip the row); it
    /// declines with the uncached sentinel, and the catch-all behind the row is
    /// not entered.
    #[test]
    fn local_handler_lookup_declines_uncached_for_a_user_loader_declaring_class() {
        const EXC: &str = "cratonvm/test/LocalHandlerForkExc";
        const DECLARING: &str = "cratonvm/test/LocalHandlerForkOwner";
        const UNLOADED: &str = "cratonvm/test/LocalHandlerForkUnloadedCatchType";
        use cratonvm_types::ClassLoaderId;
        const FORK: ClassLoaderId = ClassLoaderId::UserDefined(4343);
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let (exc, declaring) = {
            let mut cm = shared.classes.class_manager.write();
            let exc = cm
                .try_ensure_synthetic_class(EXC, 0)
                .expect("Compatible mode fabricates");
            let declaring = cm
                .try_ensure_synthetic_class(DECLARING, 0)
                .expect("Compatible mode fabricates");
            cm.class_store
                .get_mut(declaring)
                .expect("just fabricated")
                .loader_id = FORK;
            cm.register_class_name(FORK, DECLARING, declaring);
            (exc, declaring)
        };
        let site = local_handler_site(vec![(UNLOADED, 10), ("", 20)], declaring);
        assert_eq!(
            local_handler_index_slow(&shared, &site, exc),
            LOCAL_HANDLER_DECLINED
        );
        // A catch-all first is still answered without reaching the row.
        let site = local_handler_site(vec![("", 10), (UNLOADED, 20)], declaring);
        assert_eq!(local_handler_index_slow(&shared, &site, exc), 0);
    }
}

#[cfg(test)]
mod oomjit5_compiled_catch_tests {
    //! gen r4w5/oomjit5: a leftover native return must not outlive an
    //! exception that compiled code catches. See
    //! [`drain_native_return_at_compiled_catch`].
    use super::*;
    use crate::threading::ThreadId;

    #[test]
    fn a_compiled_catch_drops_a_leftover_native_return_once() {
        let mut thread = JvmThread::new(ThreadId(0), "oomjit5-catch");
        // SAFETY: non-null and 8-byte aligned, which is the whole of
        // `ObjectRef::from_raw`'s contract; the drain never dereferences it.
        let leftover = unsafe { ObjectRef::from_raw(0x10_0000usize as *mut u8) };
        thread.native_pending_return = Some(leftover);
        assert!(drain_native_return_at_compiled_catch(&mut thread, "test"));
        assert!(
            thread.native_pending_return.is_none(),
            "the handoff root must not survive the catch"
        );
        assert!(
            !drain_native_return_at_compiled_catch(&mut thread, "test"),
            "nothing left to drop the second time"
        );
    }

    /// The body of the first `fn` whose signature starts with `sig` in `src`,
    /// up to the first line that is a lone closing brace.
    fn body<'a>(src: &'a str, sig: &str) -> &'a str {
        // At a line start: other text pins quote these signatures in strings.
        let Some(start) = src.find(&format!("\n{sig}")).map(|i| i + 1) else {
            panic!("`{sig}` not found");
        };
        let rest = &src[start..];
        let end = rest.find("\n}\n").map_or(rest.len(), |e| e + 3);
        &rest[..end]
    }

    /// Source witness: every door where compiled code, or the interpreter on
    /// its behalf, enters a handler for an exception that never reaches the
    /// interpreter's own throw path (`settle_thrown_exception`) applies the
    /// drain. A door that loses it silently re-opens the leak: there is no
    /// crash, only a dropped object that stays reachable.
    #[test]
    fn every_compiled_catch_door_drains_the_leftover_native_return() {
        let helpers = crate::jit::helpers::HELPERS_SRC;
        let bridge = include_str!("../../runtime/interpreter/jit_bridge.rs");
        let dispatch = include_str!("../../runtime/interpreter/exception_dispatch.rs");
        let doors: &[(&str, &str)] = &[
            (helpers, "unsafe fn jit_local_handler_lookup_body("),
            (helpers, "unsafe fn local_handler_enter_implicit("),
            (bridge, "pub(super) fn route_osr_exception_out_of_artifact("),
            (dispatch, "pub(crate) fn run_jit_callee_handler("),
            (
                dispatch,
                "pub(super) fn route_jit_exception_through_method(",
            ),
        ];
        for (src, sig) in doors {
            assert!(
                body(src, sig).contains("drain_native_return_at_compiled_catch("),
                "`{sig}` enters a handler without draining native_pending_return"
            );
        }
    }
}

#[cfg(test)]
mod w36f_native_return_handoff_tests {
    //! gc-common w36-f (handoff-w36b): a native's object return handed back to
    //! compiled code does not stay a GC root. See
    //! [`drop_native_return_handed_to_compiled_code`].
    use super::*;
    use crate::config::VmConfig;
    use crate::threading::ThreadId;
    use std::sync::Arc;

    /// Restores the JIT thread pointer even if an assertion panics while it is
    /// installed (wave-8 lesson (c)).
    struct JitThreadInstalled(Option<JitThreadScope>);
    impl Drop for JitThreadInstalled {
        fn drop(&mut self) {
            if let Some(scope) = self.0.take() {
                restore_jit_thread(scope);
            }
        }
    }

    #[test]
    fn a_fast_native_object_return_is_not_rooted_after_the_helper_returns() {
        let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let vm_ptr = Arc::as_ptr(&shared) as i64;
        let mut thread = Box::new(JvmThread::new(ThreadId(0), "w36f-native-return"));
        let mirror = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let leftover = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        thread.java_thread_obj = Some(mirror);
        // A value an EARLIER native call returned to compiled code: the loop
        // shape of `WeakClearYoungProbe`, where it was the referent.
        thread.native_pending_return = Some(leftover);

        let installed = JitThreadInstalled(Some(set_jit_thread(&mut thread)));
        // SAFETY: `vm_ptr` names the live `SharedVm` above and the JIT thread
        // pointer is installed for this OS thread -- the contract compiled
        // code satisfies when it calls the helper.
        let bits = unsafe { jit_thread_current_thread_direct(vm_ptr) };
        drop(installed);

        assert_eq!(
            bits,
            mirror.as_ptr() as i64,
            "the fast arm answers the thread's mirror (Compatible mode, no gate)"
        );
        assert!(
            thread.native_pending_return.is_none(),
            "a native's object return handed to compiled code must not stay a \
             GC root until the next native call (handoff-w36b)"
        );
    }

    #[test]
    fn the_hand_off_drops_the_slot_and_nothing_else() {
        let mut thread = JvmThread::new(ThreadId(0), "w36f-drop");
        // SAFETY: non-null and 8-byte aligned (`ObjectRef::from_raw`'s whole
        // contract); nothing dereferences them.
        let (ret, exc) = unsafe {
            (
                ObjectRef::from_raw(0x20_0000usize as *mut u8),
                ObjectRef::from_raw(0x20_0040usize as *mut u8),
            )
        };
        thread.native_pending_return = Some(ret);
        thread.jit_pending_exception = Some(exc);
        drop_native_return_handed_to_compiled_code(&mut thread);
        assert!(thread.native_pending_return.is_none());
        assert_eq!(
            thread.jit_pending_exception,
            Some(exc),
            "a parked exception is its own root and must survive the hand-off"
        );
    }

    /// Source witness: every extern helper that can return a native's object
    /// to compiled code drops the handoff root on the way out. A wrapper that
    /// loses the call re-opens the leak silently: nothing crashes, a dropped
    /// object just stays reachable.
    #[test]
    fn every_object_returning_native_door_hands_the_root_off() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        for helper in [
            "jit_invoke_dispatch(",
            "jit_invoke_virtual_mic(",
            "jit_indy_bridge(",
            "jit_varhandle_read_direct<const SLOT: usize>(",
            "jit_integer_value_of_direct(",
            "jit_long_value_of_direct(",
            "jit_thread_current_thread_direct(",
            "jit_concurrent_hashmap_get_direct(",
            "jit_hashmap_get_direct(",
            "jit_string_latin1_to_lower_direct(",
            "jit_hashmap_put_direct(",
        ] {
            let sig = format!("extern \"C\" fn {helper}");
            let start = src
                .find(&sig)
                .unwrap_or_else(|| panic!("`{sig}` not found"));
            let rest = &src[start..];
            let body = &rest[..rest.find("\n}\n").map_or(rest.len(), |e| e + 3)];
            assert!(
                body.contains("native_return_handed_to_compiled_code();"),
                "`{helper}` returns a native's object to compiled code without \
                 dropping native_pending_return (handoff-w36b)"
            );
        }
    }
}

#[cfg(test)]
mod r11_helper_entry_native_return_tests {
    //! r11 (2026-09-25): a leftover native return must not stay a GC root
    //! across the collections a compiled loop's later helper calls run. See
    //! [`drain_native_return_at_compiled_helper_entry`] and
    //! `vm/tests/jit_alloc_oome_clears_soft_refs.rs` for the end-to-end probe.
    use super::*;
    use crate::threading::ThreadId;

    /// Through the real door: `jit_safepoint_flush_satb`, which every
    /// GC-capable helper calls first. `vm_ptr == 0` skips only the SATB half.
    #[test]
    fn a_compiled_helper_entry_drops_a_leftover_native_return() {
        let mut thread = JvmThread::new(ThreadId(0), "r11-helper-entry");
        // SAFETY: non-null and 8-byte aligned, which is the whole of
        // `ObjectRef::from_raw`'s contract; the drain never dereferences it.
        let leftover = unsafe { ObjectRef::from_raw(0x10_0000usize as *mut u8) };
        thread.native_pending_return = Some(leftover);
        let scope = set_jit_thread(&mut thread);
        // SAFETY: the JIT thread pointer is installed for this OS thread and
        // no `JitThreadGuard` is live, the contract a helper body's first
        // line runs under.
        unsafe { jit_safepoint_flush_satb(0) };
        restore_jit_thread(scope);
        assert!(
            thread.native_pending_return.is_none(),
            "a helper entered from compiled code kept the previous native \
             return rooted"
        );
    }

    /// The body of the first `fn` whose signature starts with `sig` in `src`,
    /// up to the first line that is a lone closing brace.
    fn body<'a>(src: &'a str, sig: &str) -> &'a str {
        // Anchored at a line start: other tests quote these signatures in
        // string literals, and the first bare match can be one of those.
        let Some(start) = src.find(&format!(
            "
{sig}"
        )) else {
            panic!("`{sig}` not found");
        };
        let rest = &src[start + 1..];
        let end = rest.find("\n}\n").map_or(rest.len(), |e| e + 3);
        &rest[..end]
    }

    /// t1 lane E (`CRATONVM_GC_HANDSHAKE_POLL_BIT`): the cooperative poll's
    /// slow path lowers this thread's handshake arm before `safepoint_check`
    /// drains the handshake queue, and outside the JIT-thread branch (a poll
    /// with no JIT thread must not come back at every back edge).
    #[test]
    fn t1e_the_poll_slow_path_lowers_the_handshake_arm_before_the_drain() {
        let helpers = crate::jit::helpers::HELPERS_SRC;
        let b = body(helpers, "unsafe fn jit_safepoint_slow_path_body(");
        let clear = b
            .find("gc_barrier::take_this_threads_handshake_arm();")
            .expect("the clear");
        let thread = b
            .find("if let Some((thread, _guard)) = jit_thread_mut() {\n        let hit")
            .expect("the JIT-thread branch");
        let drain = b
            .find("crate::runtime::interpreter::safepoint_check(vm.as_ref(), thread);")
            .expect("the drain");
        assert!(clear < thread && thread < drain);
    }

    /// Source witness. The drain rides the SATB flush, so it reaches every
    /// helper that flushes; the doors that matter for the soft-reference rule
    /// are pinned by name, plus the cooperative poll, which parks for a PEER's
    /// collection without flushing. Losing any of them re-opens the leak
    /// silently: nothing crashes, a dropped object just stays reachable.
    #[test]
    fn every_gc_capable_compiled_door_drains_the_leftover_native_return() {
        let helpers = crate::jit::helpers::HELPERS_SRC;
        assert!(
            body(helpers, "unsafe fn jit_safepoint_flush_satb(")
                .contains("drain_native_return_at_compiled_helper_entry();"),
            "the SATB flush door no longer drains native_pending_return"
        );
        assert!(
            body(helpers, "unsafe fn jit_safepoint_slow_path_body(")
                .contains("drain_native_return_at_compiled_helper_entry();"),
            "the cooperative safepoint poll publishes native_pending_return undrained"
        );
        for sig in [
            "unsafe fn jit_newarray_body(",
            "unsafe fn jit_new_object_body(",
            "unsafe fn jit_anewarray_object_body(",
            "unsafe fn jit_multianewarray_n_body(",
            "unsafe fn jit_invoke_dispatch_body(",
            "unsafe fn jit_invoke_virtual_mic_body(",
        ] {
            assert!(
                body(helpers, sig).contains("jit_safepoint_flush_satb(vm_ptr);"),
                "`{sig}` can collect without passing the helper-entry door"
            );
        }
    }
}

/// `CompileArgPins` (r11-irlower-outgoing-arg-staging-refs-unpublished): the
/// dispatch helpers root their reference arguments across a synchronous callee
/// compile and re-read them afterwards. The GC's part — scanning and remapping
/// `native_pin_roots` — is simulated by rewriting a pin slot, which is exactly
/// what `remap_thread_off_frame_refs` does to it.
#[cfg(test)]
mod r11w4_rt_compile_arg_pins {
    use super::*;
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::ThreadId;
    use std::sync::Arc;

    /// Receiver 0, int 1, `byte[]` 2, long 3, `String` 4, double 5.
    static MIXED_INFO: JitInvokeInfo = JitInvokeInfo {
        class_name: "p/Probe",
        method_name: "m",
        descriptor: "(I[BJLjava/lang/String;D)V",
        num_jit_args: 6,
        return_type: b'V',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    };

    /// No receiver: `Object[][]` 0, int 1, `int[]` 2.
    static STATIC_INFO: JitInvokeInfo = JitInvokeInfo {
        class_name: "p/Probe",
        method_name: "s",
        descriptor: "([[Ljava/lang/Object;I[I)V",
        num_jit_args: 3,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    };

    fn reference_positions(info: &JitInvokeInfo, len: usize) -> Vec<usize> {
        let args = vec![0i64; len];
        let mut seen = Vec::new();
        for_each_jit_reference_arg(info, &args, |index, _| seen.push(index));
        seen
    }

    /// JIT round 23 wave 3d (lane L1): the IR's direct self-call row (kind 4)
    /// has a receiver only when its argument count says so. The walk read
    /// `invoke_kind != 3`, so a STATIC self-call `(I[B)V` named its `int` a
    /// receiver and the `byte[]` at word 1 was walked as word 2 (past the
    /// buffer: missed). The decoder already answered "no receiver" for it.
    #[test]
    fn a_kind_4_row_names_a_receiver_only_when_it_carries_one() {
        let static_self = JitInvokeInfo {
            class_name: "p/Probe",
            method_name: "s",
            descriptor: "(I[B)V",
            num_jit_args: 2,
            return_type: b'V',
            invoke_kind: 4,
            declaring_class_id: 0,
            owner_class_id: 0,
        };
        let instance_self = JitInvokeInfo {
            class_name: "p/Probe",
            method_name: "m",
            descriptor: "(I[B)V",
            num_jit_args: 3,
            return_type: b'V',
            invoke_kind: 4,
            declaring_class_id: 0,
            owner_class_id: 0,
        };
        let instance_twin = JitInvokeInfo {
            invoke_kind: 1,
            ..instance_self
        };
        assert!(!jit_info_has_receiver(&static_self));
        assert_eq!(reference_positions(&static_self, 2), vec![1], "the byte[] only");
        assert!(jit_info_has_receiver(&instance_self));
        assert_eq!(reference_positions(&instance_self, 3), vec![0, 2]);
        // The kind-1 twin the VM is normally handed answers the same.
        assert!(jit_info_has_receiver(&instance_twin));
        assert_eq!(reference_positions(&instance_twin, 3), vec![0, 2]);
        // Kinds 0-3 keep `invoke_site_has_receiver`'s answer.
        assert!(jit_info_has_receiver(&MIXED_INFO));
        assert!(!jit_info_has_receiver(&STATIC_INFO));
    }

    #[test]
    fn the_reference_walk_names_the_receiver_and_every_reference_parameter() {
        assert_eq!(reference_positions(&MIXED_INFO, 6), vec![0, 2, 4]);
        assert_eq!(reference_positions(&STATIC_INFO, 3), vec![0, 2]);
        // A short buffer stops the walk instead of reading past it.
        assert_eq!(reference_positions(&MIXED_INFO, 2), vec![0]);
    }

    #[test]
    fn raw_pins_follow_a_move_and_release_to_the_watermark() {
        let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "r11w4-compile-arg-pins");
        let recv = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let arr = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let moved_to = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let outer = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        // An enclosing pin that neither round may disturb.
        thread.native_pin_roots.push(outer);
        // The `String` slot is null; the int and long slots are never pinned.
        let args = [recv.as_ptr() as i64, 64, arr.as_ptr() as i64, 7, 0, 0];

        let pins = CompileArgPins::pin_raw(&shared, &mut thread, &MIXED_INFO, &args);
        assert_eq!(thread.native_pin_roots, vec![outer, recv, arr]);
        assert_eq!(
            pins.release_raw(&mut thread, &args),
            None,
            "nothing moved, so the helper keeps reading its own buffer",
        );
        assert_eq!(thread.native_pin_roots, vec![outer]);

        let pins = CompileArgPins::pin_raw(&shared, &mut thread, &MIXED_INFO, &args);
        // The collection inside the compile moves the array.
        thread.native_pin_roots[2] = moved_to;
        let moved = pins.release_raw(&mut thread, &args);
        let mut expected = args.to_vec();
        expected[2] = moved_to.as_ptr() as i64;
        assert_eq!(
            moved,
            Some(expected),
            "the moved argument must come back at its new address, the rest unchanged",
        );
        assert_eq!(thread.native_pin_roots, vec![outer]);
    }

    #[test]
    fn value_pins_write_the_current_address_back() {
        let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "r11w4-compile-arg-pins");
        let recv = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let other = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let moved_to = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let mut values = [
            Value::Object(Some(recv)),
            Value::Int(3),
            Value::Object(None),
            Value::Object(Some(other)),
        ];

        let pins = CompileArgPins::pin_values(&shared, &mut thread, &values);
        assert_eq!(thread.native_pin_roots, vec![recv, other]);
        // The collection moves the receiver.
        thread.native_pin_roots[0] = moved_to;
        pins.release_values(&mut thread, &mut values);
        assert_eq!(
            values,
            [
                Value::Object(Some(moved_to)),
                Value::Int(3),
                Value::Object(None),
                Value::Object(Some(other)),
            ],
        );
        assert!(thread.native_pin_roots.is_empty());
    }

    /// Round 11 wave 5 (r11w4-rt-callee-rerun-reads-pre-call-argument-words):
    /// the re-run doors read the arguments back through pins that stay in
    /// place across several GC-capable steps. `current_raw` / `args_now`
    /// answer the words as they stand at each read, WITHOUT releasing, and
    /// `release` then truncates to the watermark.
    #[test]
    fn pins_read_back_current_words_without_releasing() {
        let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "r11w5-rerun-arg-pins");
        let recv = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let arr = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let moved_recv = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let moved_arr = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let outer = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        thread.native_pin_roots.push(outer);
        let args = [recv.as_ptr() as i64, 64, arr.as_ptr() as i64, 7, 0, 0];

        let pins = CompileArgPins::pin_raw(&shared, &mut thread, &MIXED_INFO, &args);
        // Nothing moved: the caller's own buffer is read, nothing is copied.
        assert!(matches!(
            args_now(Some(&pins), &thread, &args),
            std::borrow::Cow::Borrowed(_)
        ));
        // The callee's collection moves the array ...
        thread.native_pin_roots[2] = moved_arr;
        let mut expected = args.to_vec();
        expected[2] = moved_arr.as_ptr() as i64;
        assert_eq!(&*args_now(Some(&pins), &thread, &args), &expected[..]);
        // ... the pins are still there, and a later allocation moves the
        // receiver too; the next read sees both moves.
        assert_eq!(thread.native_pin_roots, vec![outer, recv, moved_arr]);
        thread.native_pin_roots[1] = moved_recv;
        expected[0] = moved_recv.as_ptr() as i64;
        assert_eq!(pins.current_raw(&thread, &args), Some(expected));
        // No pins: the buffer as given.
        assert_eq!(&*args_now(None, &thread, &args), &args[..]);
        pins.release(&mut thread);
        assert_eq!(thread.native_pin_roots, vec![outer]);
    }

    /// gen r5w4/jit8: a pin scope left WITHOUT its explicit release -- an
    /// unwind out of `compile()` / `call()`, or the `jit_thread_mut() == None`
    /// arm -- truncates to its watermark on drop. It used to leave the
    /// arguments rooted for the life of the thread when no enclosing pin scope
    /// sat below them, keeping whatever they reached alive
    /// (`docs/feature-designs/deopt-osr.md`).
    #[test]
    fn a_pin_scope_dropped_without_release_truncates_to_its_watermark() {
        let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "r5w4-jit8-pins-drop");
        let recv = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let arr = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let outer = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        thread.native_pin_roots.push(outer);
        let args = [recv.as_ptr() as i64, 64, arr.as_ptr() as i64, 7, 0, 0];

        // Raw pins, and a nested scope's leftovers above them, both dropped.
        {
            let _pins = CompileArgPins::pin_raw(&shared, &mut thread, &MIXED_INFO, &args);
            thread.native_pin_roots.push(arr);
        }
        assert_eq!(
            thread.native_pin_roots,
            vec![outer],
            "the dropped scope and everything above its watermark must be released",
        );

        // Value pins, dropped the way the `jit_thread_mut() == None` arm of
        // `compile_with_ref_args_pinned` drops them: moved into an `Option`
        // and never released. (An unwind drops them by the same `Drop`.)
        let values = [Value::Object(Some(recv)), Value::Int(1)];
        let pins = Some(CompileArgPins::pin_values(&shared, &mut thread, &values));
        assert_eq!(thread.native_pin_roots, vec![outer, recv]);
        drop(pins);
        assert_eq!(thread.native_pin_roots, vec![outer]);
    }
}

/// Interpreter round i1 wave 14, lane L2: a compiled `invoke_kind == 1` site
/// whose owner the compile substituted for the constant-pool class carries
/// that owner's id (`JitInvokeInfo::owner_class_id`), and the dispatch helper
/// takes the id instead of resolving the substituted NAME through the
/// caller's loader
/// (`interpreter-L2-substituted-owner-dispatch-resolves-the-declaring-class-by-name`).
#[cfg(test)]
mod i1w14_l2_substituted_owner_tests {
    use super::{jit_substituted_owner, substituted_owner_differs_from_by_name, JitInvokeInfo};
    use cratonvm_types::{ClassId, ClassLoaderId};

    const OWNER: &str = "cratonvm/test/i1w14l2/SplitAncestor";
    const OTHER: &str = "cratonvm/test/i1w14l2/Unrelated";
    const FORK: ClassLoaderId = ClassLoaderId::UserDefined(14_214);

    fn site(invoke_kind: u8, owner_class_id: u32) -> JitInvokeInfo {
        JitInvokeInfo {
            class_name: OWNER,
            method_name: "length",
            descriptor: "()I",
            num_jit_args: 1,
            return_type: b'I',
            invoke_kind,
            declaring_class_id: 0,
            owner_class_id,
        }
    }

    /// Two definitions of `OWNER`: the application copy the flat name map
    /// answers, and a user loader's copy (built as
    /// `a_compiled_invokestatic_resolves_its_owner_in_the_callers_own_loader`
    /// builds it), plus one unrelated class.
    fn two_copies(shared: &crate::vm::SharedVm) -> (ClassId, ClassId, ClassId) {
        let mut cm = shared.classes.class_manager.write();
        let app_copy = cm
            .try_ensure_synthetic_class(OWNER, 0)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
        let fork_copy = cm
            .try_ensure_synthetic_class("cratonvm/test/i1w14l2/SplitAncestor$Fork", 0)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
        cm.class_store
            .get_mut(fork_copy)
            .expect("just fabricated")
            .name = cratonvm_types::intern_arc(OWNER);
        cm.class_store
            .get_mut(fork_copy)
            .expect("just fabricated")
            .loader_id = FORK;
        cm.register_class_name(FORK, OWNER, fork_copy);
        let other = cm
            .try_ensure_synthetic_class(OTHER, 0)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
        (app_copy, fork_copy, other)
    }

    /// The recorded id is the answer, even when the flat name map holds the
    /// other copy of the name; nothing else is.
    #[test]
    fn a_substituted_owner_is_taken_by_its_recorded_id() {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let (app_copy, fork_copy, other) = two_copies(&shared);
        assert_ne!(app_copy, fork_copy);
        assert_eq!(
            jit_substituted_owner(&shared, &site(1, fork_copy.as_u32())),
            Some(fork_copy),
            "the id the substitution found, not the by-name copy"
        );
        assert_eq!(
            jit_substituted_owner(&shared, &site(1, app_copy.as_u32())),
            Some(app_copy)
        );
        // No substitution: the site resolves its constant-pool name as before.
        assert_eq!(jit_substituted_owner(&shared, &site(1, 0)), None);
        // Only a statically bound site carries one.
        assert_eq!(
            jit_substituted_owner(&shared, &site(0, fork_copy.as_u32())),
            None
        );
        // An id that no longer names a class called `class_name` is ignored.
        assert_eq!(
            jit_substituted_owner(&shared, &site(1, other.as_u32())),
            None
        );
        assert_eq!(jit_substituted_owner(&shared, &site(1, u32::MAX)), None);
    }

    /// The by-name compiled shortcuts are declined exactly when the flat name
    /// map answers another copy than the recorded owner.
    #[test]
    fn the_by_name_shortcuts_decline_a_substituted_owners_other_copy() {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let (app_copy, fork_copy, _) = two_copies(&shared);
        let cm = shared.classes.class_manager.read();
        assert!(
            matches!(
                cm.classify_loaded_name(OWNER),
                cratonvm_classloading::NameResolution::Ambiguous { .. }
            ),
            "the fixture must produce two definitions of one name"
        );
        let by_name = cm.get_loaded_class_id(OWNER);
        let (same, differs) = if by_name == Some(app_copy) {
            (app_copy, fork_copy)
        } else {
            (fork_copy, app_copy)
        };
        assert!(substituted_owner_differs_from_by_name(
            &cm,
            &site(1, differs.as_u32())
        ));
        assert!(!substituted_owner_differs_from_by_name(
            &cm,
            &site(1, same.as_u32())
        ));
        assert!(!substituted_owner_differs_from_by_name(&cm, &site(1, 0)));
    }
}

/// Interpreter round i1 wave 17, lane L1
/// (`i15-L3-proposal-poll-slow-path-names-its-compiled-body`): the safepoint
/// slow path trusts the body id a poll passes only when this thread's
/// compile-id mirror confirms it.
#[cfg(test)]
mod i17_l1_polling_body_id_tests {
    use super::polling_body_id;

    #[test]
    fn a_poll_names_its_body_only_when_the_mirror_agrees() {
        assert_eq!(polling_body_id(7, 7), Some(7));
        assert_eq!(polling_body_id(0, 0), None, "0 is no body");
        assert_eq!(polling_body_id(7, 0), None, "nothing published");
        assert_eq!(polling_body_id(7, 8), None, "another frame's identity");
        // An undefined register (the IR and aarch64 polls do not load it).
        assert_eq!(polling_body_id(-1, u32::MAX), None);
        assert_eq!(polling_body_id(i64::from(u32::MAX) + 7, 7), None);
    }
}

/// Interpreter round i1 wave 24, lane L6: the safepoint slow path names an
/// optimizing OSR body that was never published. Its compile id is reserved
/// by the lowerer and handed to the artifact, but only `JitCache::put` /
/// `put_osr` bind it, so `lookup_compile_id` answers `None` -- which is why
/// wave 23's withdrawn OSR body of `RedefineRunningSpliceProbe.spin` was never
/// told to leave. The OSR door's continuation record names it instead.
#[cfg(test)]
mod i24_l6_unpublished_osr_body_tests {
    use super::osr_continuation_body_named;
    use std::sync::Arc;

    #[test]
    fn an_unpublished_osr_body_is_named_by_its_continuation_record() {
        let id = cratonvm_jit::reserve_compile_id();
        assert_ne!(id, 0, "an id is available");
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&[0xC3]); // RET
        let mut body = cratonvm_jit::CompiledMethod::new(buf);
        // What `ir_lower` does with its reservation (`hand_off`); the body
        // is then remembered by the OSR memo, never published.
        body.compile_id = id;
        let body = Arc::new(body);
        // Cast: the continuation record's encoding of the artifact.
        let ptr = Arc::as_ptr(&body) as usize;
        assert_eq!(
            cratonvm_jit::lookup_compile_id(id),
            None,
            "reserved, never bound: the lookup wave 23 relied on misses it"
        );
        assert_eq!(osr_continuation_body_named(1, id), None, "no OSR entry");
        let named =
            crate::runtime::interpreter::jit_bridge::with_osr_continuation_for_test(0, ptr, || {
                (
                    osr_continuation_body_named(1, id),
                    osr_continuation_body_named(1, id.wrapping_add(1)),
                    osr_continuation_body_named(2, id),
                    osr_continuation_body_named(0, id),
                )
            });
        assert_eq!(named.0, Some(ptr), "the innermost frame's OSR body");
        assert_eq!(named.1, None, "a compiled callee of it has another id");
        assert_eq!(named.2, None, "the innermost frame is not the OSR'd one");
        assert_eq!(named.3, None, "no frame at all");
        assert_eq!(
            osr_continuation_body_named(1, id),
            None,
            "withdrawn on exit"
        );
        drop(body);
    }
}

/// Interpreter round i1 wave 18, lane L4: the helper memos no longer switch
/// off for the process on the first redefinition anywhere.
#[cfg(test)]
mod i18_l4_redefinition_memo_tests {
    use super::{
        callee_template_declines_redefined_target, callee_templates_latched_off, jit_site_key,
        ldc_slot_memo_latched_off, try_jit_static_bytecode_callee, typecheck_memos_latched_off,
        ClassId, JitInvokeInfo, Value, STATIC_BYTECODE_CALLEE_CACHE,
    };
    use crate::classloading::resolution::{CachedBytecodeMethod, RedefineGate};
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use crate::vm::SharedVm;
    use std::sync::atomic::AtomicU32;
    use std::sync::Arc;

    static SITE: JitInvokeInfo = JitInvokeInfo {
        class_name: "cratonvm/test/I18L4Callee",
        method_name: "twice",
        descriptor: "(I)I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    };

    /// None of the memos is latched: whether a class was ever redefined in
    /// this process does not decide whether they serve.
    #[test]
    fn no_helper_memo_is_latched_off_by_a_redefinition() {
        assert!(!ldc_slot_memo_latched_off());
        assert!(!typecheck_memos_latched_off());
        assert!(!callee_templates_latched_off());
        assert!(!callee_template_declines_redefined_target(0));
        // Wave 40 (lane L2): a template of a class redefined before its fill
        // is served, unless the kill switch restores the wave-18 decline.
        assert_eq!(
            callee_template_declines_redefined_target(1),
            !super::CALLEE_TEMPLATES_SERVE_REDEFINED_TARGETS
        );
    }

    /// Under `CALLEE_TEMPLATES_SERVE_REDEFINED_TARGETS`'s kill switch, a
    /// template whose declaring class was redefined is current (its gate is
    /// not stale: filled after the redefinition) but is still not served:
    /// that callee keeps the by-name path. It is kept, not evicted, so the
    /// site does not re-resolve on every call. (With the switch on, the
    /// default since wave 40, it is served and the call runs, which this
    /// stripped thread cannot do.)
    #[test]
    fn a_template_of_a_redefined_callee_is_kept_but_not_served() {
        if !crate::runtime::env_cache::jit_static_bytecode_callee()
            || !callee_template_declines_redefined_target(1)
        {
            return;
        }
        let vm = SharedVm::new(VmConfig::default());
        let key = jit_site_key(vm.vm_identity, &SITE as *const JitInvokeInfo as usize);
        let cached = Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: ClassId::new(0x00AB_1811),
                class_name: Arc::from("cratonvm/test/I18L4Callee"),
                method_name: Arc::from("twice"),
                method_descriptor: Arc::from("(I)I"),
                source_file: None,
                // iload_0; iload_0; iadd; ireturn
                code: crate::runtime::frame::padded_bytecode(&[0x1a, 0x1a, 0x60, 0xac]),
                exception_table: Arc::from(Vec::new()),
                max_stack: 2,
                max_locals: 1,
                num_params: 1,
                is_synchronized: false,
                is_static: true,
            },
        ));
        // Redefined once, and the entry was filled after that: not stale.
        let gate = RedefineGate::snapshot(Arc::new(AtomicU32::new(1)));
        assert!(!gate.is_stale());
        STATIC_BYTECODE_CALLEE_CACHE.with(|c| {
            c.borrow_mut().insert(key, Some((cached, gate)));
        });
        let mut thread = JvmThread::new(ThreadId(18_418), "i18-l4-template");
        // SAFETY: `SITE` is a static `JitInvokeInfo`; the call declines before
        // it reads the thread or the arguments.
        let args = [Value::Int(3)];
        let served = unsafe { try_jit_static_bytecode_callee(&vm, &mut thread, &SITE, key, &args) };
        assert!(
            served.is_none(),
            "a redefined callee keeps the by-name path"
        );
        assert!(
            STATIC_BYTECODE_CALLEE_CACHE.with(|c| c.borrow().contains_key(&key)),
            "a current template is kept"
        );
        STATIC_BYTECODE_CALLEE_CACHE.with(|c| {
            c.borrow_mut().remove(&key);
        });
    }
}

/// Interpreter round i1 wave 20, lane L1
/// (`i19-L3-compiled-bodies-of-a-redefined-class-resolve-old-indices-in-the-new-pool`):
/// a compiled `(holder, cp index)` helper site stamped before its class's
/// redefinition reads its own constant through the class's redefinition
/// history; a site compiled after it, or carrying no stamp, reads its index
/// as it is.
#[cfg(test)]
mod i20_l1_cp_site_index_tests {
    use super::*;
    use cratonvm_classloading::{ClassLoaderId, DefineClassOptions, RedefineOptions};

    /// A version-52 class `i20l1/CpStamp` whose one method `static value()I`
    /// is `ldc #8; ireturn`, `#8` being `CONSTANT_Integer constant` (the
    /// shape `interpreter::obsolete_frames`' tests use).
    fn ldc_class(constant: i32) -> Vec<u8> {
        fn utf8(out: &mut Vec<u8>, s: &str) {
            out.push(1);
            out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
        b.extend_from_slice(&9u16.to_be_bytes()); // constant_pool_count
        utf8(&mut b, "i20l1/CpStamp"); // #1
        b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
        utf8(&mut b, "java/lang/Object"); // #3
        b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
        utf8(&mut b, "value"); // #5
        utf8(&mut b, "()I"); // #6
        utf8(&mut b, "Code"); // #7
        b.push(3); // #8 Integer
        b.extend_from_slice(&constant.to_be_bytes());
        b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
        b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
        b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
        b.extend_from_slice(&[0, 1]); // methods_count
        b.extend_from_slice(&[0x00, 0x09, 0, 5, 0, 6, 0, 1]); // public static value()I
        let code = [0x12, 8, 0xac]; // ldc #8; ireturn
        b.extend_from_slice(&[0, 7]); // "Code"
        b.extend_from_slice(&(12 + code.len() as u32).to_be_bytes());
        b.extend_from_slice(&[0, 1, 0, 0]); // max_stack 1, max_locals 0
        b.extend_from_slice(&(code.len() as u32).to_be_bytes());
        b.extend_from_slice(&code);
        b.extend_from_slice(&[0, 0, 0, 0]); // no handlers, no attributes
        b.extend_from_slice(&[0, 0]); // no class attributes
        b
    }

    /// The word compiled code bakes (`cratonvm_jit::cp_holder_word`) for
    /// code compiled at `stamp`.
    fn word(cid: ClassId, stamp: u64) -> i64 {
        // Cast: the packed word as the helper ABI's i64 bit pattern.
        (u64::from(cid.as_u32()) | ((stamp + 1) << 32)) as i64
    }

    #[test]
    fn a_site_compiled_before_its_class_was_redefined_reads_its_own_constant() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let cid = {
            let mut cm = shared.classes.class_manager.write();
            match cm.define_class_with_options(
                "i20l1/CpStamp",
                &ldc_class(70_000),
                ClassLoaderId::Application,
                DefineClassOptions::default(),
            ) {
                Ok(cid) => cid,
                // The stripped test VM could not define it: nothing to run.
                Err(_) => return,
            }
        };
        let compiled_at = crate::classloading::class_redefinition_count();
        assert_eq!(cp_site_index(&shared, word(cid, compiled_at), 8), (cid, 8));
        assert_eq!(
            cp_site_index(&shared, i64::from(cid.as_u32()), 8),
            (cid, 8),
            "a word with no stamp reads its index as it is"
        );
        shared
            .classes
            .class_manager
            .write()
            .redefine_class(cid, ldc_class(80_000), RedefineOptions::default())
            .expect("a same-shape redefinition succeeds");

        let (holder, idx) = cp_site_index(&shared, word(cid, compiled_at), 8);
        assert_eq!(holder, cid);
        assert_ne!(idx, 8, "#8 is the new constant now");
        {
            let cm = shared.classes.class_manager.read();
            let pool = &cm.get_class(cid).expect("loaded").constant_pool;
            assert!(matches!(
                pool.get(idx),
                Some(cratonvm_reader::constant_pool::ConstantPoolEntry::Integer(
                    70_000
                ))
            ));
            assert!(matches!(
                pool.get(8),
                Some(cratonvm_reader::constant_pool::ConstantPoolEntry::Integer(
                    80_000
                ))
            ));
        }
        let fresh = crate::classloading::class_redefinition_count();
        assert_eq!(
            cp_site_index(&shared, word(cid, fresh), 8),
            (cid, 8),
            "code compiled after the redefinition indexes the current pool"
        );
    }

    /// Interpreter round i1 wave 21, lane L1
    /// (`interpreter-L1-other-cp-indexed-jit-helpers-carry-no-cp-stamp-FIXED`): a
    /// `multianewarray` site word carries a truncated stamp beside its cp
    /// index, and `jit_multianewarray_n` translates the index the same way:
    /// a site packed before the redefinition reads its own constant, one
    /// packed after it or with no stamp reads `#8`.
    #[test]
    fn a_multianewarray_site_packed_before_a_redefinition_reads_its_own_constant() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let cid = {
            let mut cm = shared.classes.class_manager.write();
            // Its own VM, so the name the test above defines is free here.
            match cm.define_class_with_options(
                "i20l1/CpStamp",
                &ldc_class(70_000),
                ClassLoaderId::Application,
                DefineClassOptions::default(),
            ) {
                Ok(cid) => cid,
                // The stripped test VM could not define it: nothing to run.
                Err(_) => return,
            }
        };
        let site_at = |stamp: u64| -> i64 {
            // Cast: the packed word as the helper ABI's i64 bit pattern.
            (u64::from(cid.as_u32())
                | (8_u64 << 32)
                | ((stamp % cratonvm_jit::MULTIANEWARRAY_STAMP_MODULUS + 1) << 48))
                as i64
        };
        let unstamped = (u64::from(cid.as_u32()) | (8_u64 << 32)) as i64;
        let compiled_at = crate::classloading::class_redefinition_count();
        let before = site_at(compiled_at);
        assert_eq!(
            cratonvm_jit::unpack_multianewarray_site(before),
            (cid.as_u32(), 8)
        );
        assert_eq!(multianewarray_site(&shared, before).1, 8);
        shared
            .classes
            .class_manager
            .write()
            .redefine_class(cid, ldc_class(80_000), RedefineOptions::default())
            .expect("a same-shape redefinition succeeds");

        let idx = multianewarray_site(&shared, before).1;
        assert_ne!(idx, 8, "#8 is the new constant now");
        {
            let cm = shared.classes.class_manager.read();
            let pool = &cm.get_class(cid).expect("loaded").constant_pool;
            assert!(matches!(
                pool.get(idx),
                Some(cratonvm_reader::constant_pool::ConstantPoolEntry::Integer(
                    70_000
                ))
            ));
        }
        let fresh = crate::classloading::class_redefinition_count();
        assert_eq!(multianewarray_site(&shared, site_at(fresh)).1, 8);
        assert_eq!(
            multianewarray_site(&shared, unstamped).1,
            8,
            "a word with no stamp reads its index as it is"
        );
    }

    /// Interpreter round i1 wave 44, lane L2
    /// (`interpreter-L2-multianewarray-and-type-check-sites-judge-their-index-before-the-pool-read-FIXED-20261008`):
    /// a `multianewarray` site judged lock-free just before a redefinition
    /// that renumbers its entry -- the redefinition landing between the
    /// helper's count read and its pool read -- no longer trusts the probe:
    /// the verdict fails, and the index judged under the guard the pool is
    /// read under (`CompiledMultiANewArraySite::index_in`) names the site's
    /// own constant, where the probe index names the new one.
    #[test]
    fn a_multianewarray_site_judged_before_a_redefinition_reads_its_constant_at_the_guarded_index()
    {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let cid = {
            let mut cm = shared.classes.class_manager.write();
            // Its own VM, so the name the tests above define is free here.
            match cm.define_class_with_options(
                "i20l1/CpStamp",
                &ldc_class(70_000),
                ClassLoaderId::Application,
                DefineClassOptions::default(),
            ) {
                Ok(cid) => cid,
                // The stripped test VM could not define it: nothing to run.
                Err(_) => return,
            }
        };
        let compiled_at = crate::classloading::class_redefinition_count();
        // Cast: the packed word as the helper ABI's i64 bit pattern.
        let site = (u64::from(cid.as_u32())
            | (8_u64 << 32)
            | ((compiled_at % cratonvm_jit::MULTIANEWARRAY_STAMP_MODULUS + 1) << 48))
            as i64;
        let (cp_site, probe, judged_at) = multianewarray_site(&shared, site);
        assert_eq!(probe, 8, "no redefinition yet: the baked index");
        shared
            .classes
            .class_manager
            .write()
            .redefine_class(cid, ldc_class(80_000), RedefineOptions::default())
            .expect("a same-shape redefinition succeeds");
        assert!(
            !CpSite::verdict_still_holds(judged_at),
            "a probe judged before the redefinition is not kept"
        );
        let cm = shared.classes.class_manager.read();
        let guarded = cp_site.index_in(&cm);
        assert_ne!(guarded, probe, "#8 is the new constant now");
        let pool = &cm.get_class(cid).expect("loaded").constant_pool;
        assert!(matches!(
            pool.get(guarded),
            Some(cratonvm_reader::constant_pool::ConstantPoolEntry::Integer(
                70_000
            ))
        ));
        assert!(matches!(
            pool.get(probe),
            Some(cratonvm_reader::constant_pool::ConstantPoolEntry::Integer(
                80_000
            ))
        ));
    }

    /// `ldc_class` with `#8` a `CONSTANT_Class` naming `target` (`#9` its
    /// name) and `value()Ljava/lang/Object;` returning it.
    fn class_constant_class(target: &str) -> Vec<u8> {
        fn utf8(out: &mut Vec<u8>, s: &str) {
            out.push(1);
            out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
        b.extend_from_slice(&10u16.to_be_bytes()); // constant_pool_count
        utf8(&mut b, "i20l1/CpStamp"); // #1
        b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
        utf8(&mut b, "java/lang/Object"); // #3
        b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
        utf8(&mut b, "value"); // #5
        utf8(&mut b, "()Ljava/lang/Object;"); // #6
        utf8(&mut b, "Code"); // #7
        b.extend_from_slice(&[7, 0, 9]); // #8 Class #9
        utf8(&mut b, target); // #9
        b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
        b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
        b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
        b.extend_from_slice(&[0, 1]); // methods_count
        b.extend_from_slice(&[0x00, 0x09, 0, 5, 0, 6, 0, 1]); // public static value()
        let code = [0x12, 8, 0xb0]; // ldc #8; areturn
        b.extend_from_slice(&[0, 7]); // "Code"
        b.extend_from_slice(&(12 + code.len() as u32).to_be_bytes());
        b.extend_from_slice(&[0, 1, 0, 0]); // max_stack 1, max_locals 0
        b.extend_from_slice(&(code.len() as u32).to_be_bytes());
        b.extend_from_slice(&code);
        b.extend_from_slice(&[0, 0, 0, 0]); // no handlers, no attributes
        b.extend_from_slice(&[0, 0]); // no class attributes
        b
    }

    /// Interpreter round i1 wave 21, lane L1: a holder-keyed type-check site
    /// of code compiled before its holder's redefinition keys its resolution
    /// record by the entry that still names its class; a translated entry
    /// naming another class, a site compiled after the redefinition, and a
    /// word with no stamp keep the index as it is.
    #[test]
    fn a_typecheck_site_compiled_before_a_redefinition_keys_its_own_class_entry() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let cid = {
            let mut cm = shared.classes.class_manager.write();
            match cm.define_class_with_options(
                "i20l1/CpStamp",
                &class_constant_class("i21l1/AaA"),
                ClassLoaderId::Application,
                DefineClassOptions::default(),
            ) {
                Ok(cid) => cid,
                // The stripped test VM could not define it: nothing to run.
                Err(_) => return,
            }
        };
        let compiled_at = crate::classloading::class_redefinition_count();
        let before = u64::from(cid.as_u32()) | ((compiled_at + 1) << 32);
        // Judged under the guard, as the helper judges it (wave 44).
        let judge = |word: u64, name: &str| {
            typecheck_site_index_in(&shared.classes.class_manager.read(), word, 8, name)
        };
        assert_eq!(judge(before, "i21l1/AaA"), (cid, 8));
        if shared
            .classes
            .class_manager
            .write()
            .redefine_class(
                cid,
                class_constant_class("i21l1/BbB"),
                RedefineOptions::default(),
            )
            .is_err()
        {
            // A redefinition this stripped VM refuses: nothing to translate.
            return;
        }
        let (holder, idx) = judge(before, "i21l1/AaA");
        assert_eq!(holder, cid);
        assert_ne!(idx, 8, "#8 names the new class now");
        {
            let cm = shared.classes.class_manager.read();
            let pool = &cm.get_class(cid).expect("loaded").constant_pool;
            assert_eq!(pool.get_class_name(idx), Some("i21l1/AaA"));
            assert_eq!(pool.get_class_name(8), Some("i21l1/BbB"));
        }
        assert_eq!(
            judge(before, "i21l1/BbB"),
            (cid, 8),
            "a translation naming another class is not taken"
        );
        let fresh = crate::classloading::class_redefinition_count();
        let after = u64::from(cid.as_u32()) | ((fresh + 1) << 32);
        assert_eq!(judge(after, "i21l1/BbB"), (cid, 8));
        assert_eq!(
            judge(u64::from(cid.as_u32()), "i21l1/AaA"),
            (cid, 8),
            "a word with no stamp reads its index as it is"
        );
    }
}

/// gc-common w36-a (`common-w22b-md-update-fastpath-class-id-is-process-wide`):
/// the JIT's `MessageDigest.update(byte)` fast path judges a receiver's class
/// in the asking VM. The VM identities are private to this module; the memo is
/// per thread, so sibling tests cannot disturb it.
#[cfg(test)]
mod w36a_md_class_verdict_tests {
    use super::*;

    /// VM A's `MessageDigest` has class id 17; in VM B id 17 is BouncyCastle's
    /// digest subclass and its own `MessageDigest` is 23. The process-wide id
    /// latched 17 for both VMs: B's subclass was "ours" (its bytes went into
    /// another digest's accumulator) and B's own digest never was.
    #[test]
    fn a_class_id_is_judged_in_the_asking_vm() {
        const VM_A: usize = 0xA36_0001;
        const VM_B: usize = 0xA36_0002;
        let table = |vm: usize, id: u32| -> Option<bool> {
            let name = match (vm, id) {
                (VM_A, 17) | (VM_B, 23) => "java/security/MessageDigest",
                (VM_B, 17) => "org/bouncycastle/jcajce/provider/digest/SHA256$Digest",
                _ => return None,
            };
            Some(
                cratonvm_native_builtins::jca::message_digest::update_fastpath::class_name_is_ours(
                    name,
                ),
            )
        };
        assert!(md_class_verdict_memoised(VM_A, 17, || table(VM_A, 17)));
        // Straight after A's verdict is memoised, B asks about the same id.
        assert!(
            !md_class_verdict_memoised(VM_B, 17, || table(VM_B, 17)),
            "VM B's provider subclass was judged by VM A's class id"
        );
        assert!(
            md_class_verdict_memoised(VM_B, 23, || table(VM_B, 23)),
            "VM B's own MessageDigest was refused"
        );
        // A memo hit does not resolve again.
        assert!(md_class_verdict_memoised(VM_B, 23, || unreachable!(
            "the memoised verdict was re-resolved"
        )));
        assert!(!md_class_verdict_memoised(VM_A, 23, || table(VM_A, 23)));
        // Class id 0 (an array, or unresolved) and an unknown class are never
        // ours, and an unknown class is not memoised.
        assert!(!md_class_verdict_memoised(VM_A, 0, || Some(true)));
        assert!(!md_class_verdict_memoised(VM_A, 99, || None));
        assert!(md_class_verdict_memoised(VM_A, 99, || Some(true)));
    }
}

/// Round 12 wave 1 (lane rt): the per-site quiesce-verdict memo
/// (`site_class_was_redefined`) is served while the VM's redefinition epoch
/// holds and recomputed the moment it moves.
#[cfg(test)]
mod r12_rt_quiesce_verdict_memo_tests {
    use super::*;
    use cratonvm_classloading::{ClassLoaderId, DefineClassOptions, RedefineOptions};

    /// A version-52 class `name` whose one method `static value()I` is
    /// `ldc #8; ireturn`, `#8` being `CONSTANT_Integer constant`.
    fn ldc_class(name: &str, constant: i32) -> Vec<u8> {
        fn utf8(out: &mut Vec<u8>, s: &str) {
            out.push(1);
            out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
        b.extend_from_slice(&9u16.to_be_bytes()); // constant_pool_count
        utf8(&mut b, name); // #1
        b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
        utf8(&mut b, "java/lang/Object"); // #3
        b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
        utf8(&mut b, "value"); // #5
        utf8(&mut b, "()I"); // #6
        utf8(&mut b, "Code"); // #7
        b.push(3); // #8 Integer
        b.extend_from_slice(&constant.to_be_bytes());
        b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
        b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
        b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
        b.extend_from_slice(&[0, 1]); // methods_count
        b.extend_from_slice(&[0x00, 0x09, 0, 5, 0, 6, 0, 1]); // public static value()I
        let code = [0x12, 8, 0xac]; // ldc #8; ireturn
        b.extend_from_slice(&[0, 7]); // "Code"
        b.extend_from_slice(&(12 + code.len() as u32).to_be_bytes());
        b.extend_from_slice(&[0, 1, 0, 0]); // max_stack 1, max_locals 0
        b.extend_from_slice(&(code.len() as u32).to_be_bytes());
        b.extend_from_slice(&code);
        b.extend_from_slice(&[0, 0, 0, 0]); // no handlers, no attributes
        b.extend_from_slice(&[0, 0]); // no class attributes
        b
    }

    fn define(shared: &SharedVm, name: &str) -> Option<ClassId> {
        shared
            .classes
            .class_manager
            .write()
            .define_class_with_options(
                name,
                &ldc_class(name, 1),
                ClassLoaderId::Application,
                DefineClassOptions::default(),
            )
            .ok()
    }

    /// What `redefine_class_with` does, in its order: the class's generation
    /// under the writer, then the VM's redefinition epoch.
    fn redefine(shared: &SharedVm, cid: ClassId, name: &str) {
        shared
            .classes
            .class_manager
            .write()
            .redefine_class(cid, ldc_class(name, 2), RedefineOptions::default())
            .expect("a same-shape redefinition succeeds");
        shared.jit.jit_cache.bump_redefine_epoch();
    }

    #[test]
    fn a_site_quiesce_verdict_is_memoized_until_the_redefine_epoch_moves() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let (Some(site_cid), Some(other_cid)) = (
            define(&shared, "r12rt/QuiesceSite"),
            define(&shared, "r12rt/QuiesceOther"),
        ) else {
            // The stripped test VM could not define them: nothing to run.
            return;
        };
        // Some OTHER class redefined: the process-wide flag is up, the site's
        // own class is not redefined.
        redefine(&shared, other_cid, "r12rt/QuiesceOther");
        assert!(crate::classloading::any_class_redefined());

        let info = JitInvokeInfo {
            class_name: "r12rt/QuiesceSite",
            method_name: "value",
            descriptor: "()I",
            num_jit_args: 0,
            return_type: b'I',
            invoke_kind: 3,
            declaring_class_id: site_cid.as_u32(),
            owner_class_id: 0,
        };
        let key = jit_site_key(shared.vm_identity, &info as *const JitInvokeInfo as usize);
        QUIESCE_VERDICT.with(|m| m.borrow_mut().clear());

        assert!(!site_class_was_redefined(&shared, &info, key));
        let epoch = shared.jit.jit_cache.redefine_epoch();
        assert_eq!(
            QUIESCE_VERDICT.with(|m| m.borrow().get(&key).copied()),
            Some((epoch, false)),
            "the verdict is memoized at the epoch read before it"
        );
        // Served from the memo while the epoch holds (a planted `true` proves
        // no lookup ran).
        QUIESCE_VERDICT.with(|m| {
            m.borrow_mut().insert(key, (epoch, true));
        });
        assert!(site_class_was_redefined(&shared, &info, key));
        // A `false` from an older epoch is never served once the site's own
        // class was redefined.
        QUIESCE_VERDICT.with(|m| {
            m.borrow_mut().insert(key, (epoch, false));
        });
        redefine(&shared, site_cid, "r12rt/QuiesceSite");
        assert!(
            site_class_was_redefined(&shared, &info, key),
            "a stale-epoch `false` survived the site's own redefinition"
        );
        assert_eq!(
            QUIESCE_VERDICT.with(|m| m.borrow().get(&key).copied()),
            Some((shared.jit.jit_cache.redefine_epoch(), true))
        );
        QUIESCE_VERDICT.with(|m| m.borrow_mut().clear());
    }

    /// Round 12 wave 2 (lane rt, R12W1-1): a redefined class's site stands its
    /// compiled-entry arms down until this thread's quiesced-site flush has
    /// run at the current redefinition epoch, and again after each later
    /// redefinition; a site whose class was never redefined never does.
    #[test]
    fn a_redefined_site_is_quiesced_once_per_thread_per_redefinition() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let (Some(site_cid), Some(other_cid)) = (
            define(&shared, "r12w2rt/QuiesceGenSite"),
            define(&shared, "r12w2rt/QuiesceGenOther"),
        ) else {
            return;
        };
        let info_for = |name: &'static str, cid: ClassId| JitInvokeInfo {
            class_name: name,
            method_name: "value",
            descriptor: "()I",
            num_jit_args: 0,
            return_type: b'I',
            invoke_kind: 3,
            declaring_class_id: cid.as_u32(),
            owner_class_id: 0,
        };
        let site = info_for("r12w2rt/QuiesceGenSite", site_cid);
        let other = info_for("r12w2rt/QuiesceGenOther", other_cid);
        let key_of = |i: &JitInvokeInfo| {
            jit_site_key(shared.vm_identity, i as *const JitInvokeInfo as usize)
        };
        let (site_key, other_key) = (key_of(&site), key_of(&other));
        QUIESCE_VERDICT.with(|m| m.borrow_mut().clear());
        QUIESCED_SITE_FLUSH_STAMP.with(|s| s.set((0, u32::MAX)));

        redefine(&shared, site_cid, "r12w2rt/QuiesceGenSite");
        assert!(site_quiesced_for_redefinition(&shared, &site, site_key));
        assert!(
            !site_quiesced_for_redefinition(&shared, &other, other_key),
            "a class never redefined is never quiesced"
        );
        if !quiesce_by_generation_enabled() {
            QUIESCE_VERDICT.with(|m| m.borrow_mut().clear());
            return;
        }
        // What `jit_invoke_dispatch_body` does on that quiesced call.
        let epoch = shared.jit.jit_cache.redefine_epoch();
        QUIESCED_SITE_FLUSH_STAMP.with(|s| s.set((shared.vm_identity, epoch)));
        assert!(
            !site_quiesced_for_redefinition(&shared, &site, site_key),
            "flushed at this epoch: the compiled arms serve the site again"
        );
        assert!(
            site_class_was_redefined(&shared, &site, site_key),
            "the intrinsic shortcuts still see the class as redefined"
        );
        redefine(&shared, site_cid, "r12w2rt/QuiesceGenSite");
        assert!(
            site_quiesced_for_redefinition(&shared, &site, site_key),
            "a later redefinition quiesces the site again"
        );
        QUIESCED_SITE_FLUSH_STAMP.with(|s| s.set((0, u32::MAX)));
        QUIESCE_VERDICT.with(|m| m.borrow_mut().clear());
    }

    /// A body of `value()I` owning one deopt point, and that point's address
    /// (the boxed point keeps its address when the body moves into the cache).
    fn body_with_a_point() -> (cratonvm_jit::CompiledMethod, usize) {
        use cratonvm_jit::deopt::{
            DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, ResumeSemantics,
        };
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&[0xC3]); // RET
        let mut body = cratonvm_jit::CompiledMethod::new(buf);
        let point = Box::new(DeoptimizationPoint {
            native_offset: 0,
            bci: 0,
            reason: DeoptReason::ClassCheck,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: FrameState {
                method_key: String::new(),
                bci: 0,
                locals: Vec::new(),
                stack: Vec::new(),
                monitors: Vec::new(),
                caller: None,
            },
            semantics: ResumeSemantics::REEXECUTE,
        });
        let addr = &*point as *const DeoptimizationPoint as usize;
        body._deopt_point_boxes.push(point);
        (body, addr)
    }

    /// Round 12 wave 2 (lane rt): a trap stashed by a body of a redefined class
    /// resumes only when the body published for the method owns its point and
    /// was compiled after the class's last redefinition; a body compiled
    /// before it, an unknown point and a point no published body owns are
    /// refused, as before.
    #[test]
    fn a_trap_of_a_redefined_class_resumes_only_out_of_a_current_body() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let name = "r12w2rt/ResumeGate";
        let Some(cid) = define(&shared, name) else {
            return;
        };
        let publish = |body| {
            shared.jit.jit_cache.put(
                std::sync::Arc::from(name),
                std::sync::Arc::from("value"),
                std::sync::Arc::from("()I"),
                cid,
                body,
            )
        };
        let (old_body, old_point) = body_with_a_point();
        publish(old_body);
        // Held, as the call arm that entered a trapping body holds it, so the
        // new body's point cannot reuse the old one's address.
        let old_alive = shared.jit.jit_cache.get(name, "value", "()I", cid);
        assert!(old_alive.is_some(), "the old body was published");
        assert!(
            !stashed_callee_predates_redefinition(
                &shared,
                cid.as_u32(),
                name,
                "value",
                "()I",
                old_point
            ),
            "a class never redefined is not refused on this account"
        );
        // The production order: the class under the writer, then the JIT side
        // (withdrawal and barrier) through `note_class_redefinition_of`.
        shared
            .classes
            .class_manager
            .write()
            .redefine_class(cid, ldc_class(name, 2), RedefineOptions::default())
            .expect("a same-shape redefinition succeeds");
        shared.jit.note_class_redefinition_of(cid, name, false);
        assert!(
            stashed_callee_predates_redefinition(
                &shared,
                cid.as_u32(),
                name,
                "value",
                "()I",
                old_point
            ),
            "the body compiled before the redefinition ran the old bytecode"
        );
        assert!(
            stashed_callee_predates_redefinition(&shared, cid.as_u32(), name, "value", "()I", 0),
            "a trap with no point cannot name its body"
        );
        let (new_body, new_point) = body_with_a_point();
        publish(new_body);
        if quiesce_by_generation_enabled() {
            assert!(
                !stashed_callee_predates_redefinition(
                    &shared,
                    cid.as_u32(),
                    name,
                    "value",
                    "()I",
                    new_point
                ),
                "the current body's trap resumes against the bytecode it ran"
            );
        }
        assert!(
            stashed_callee_predates_redefinition(
                &shared,
                cid.as_u32(),
                name,
                "value",
                "()I",
                old_point
            ),
            "the published body does not own the old body's point"
        );
    }

    /// Interpreter round i1 wave 29, lane L3: the class a trap is judged by
    /// is the one the call site's loader resolves. With one name defined by
    /// the application loader (never redefined) and by a user loader
    /// (redefined), a stale body of the user loader's class used to be judged
    /// by the application loader's class (`get_loaded_class_id` answers the
    /// built-in loaders first) and its trap rebuilt against the NEW bytecode.
    #[test]
    fn a_trap_is_judged_by_the_class_the_sites_loader_resolves() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let name = "r29l3/TwoLoaders";
        let Some(app_cid) = define(&shared, name) else {
            return;
        };
        let user_cid = match shared
            .classes
            .class_manager
            .write()
            .define_class_with_options(
                name,
                &ldc_class(name, 1),
                ClassLoaderId::UserDefined(29_003),
                DefineClassOptions::default(),
            ) {
            Ok(cid) => cid,
            Err(_) => return,
        };
        let resolves_own = shared
            .classes
            .class_manager
            .read()
            .find_class_by_name_for_class(name, user_cid)
            == Some(user_cid);
        if app_cid == user_cid || !resolves_own {
            // This build's resolution policy does not give the user loader
            // its own class back: nothing to tell apart.
            return;
        }
        let (old_body, old_point) = body_with_a_point();
        shared.jit.jit_cache.put(
            std::sync::Arc::from(name),
            std::sync::Arc::from("value"),
            std::sync::Arc::from("()I"),
            user_cid,
            old_body,
        );
        // Held, as the call arm that entered a trapping body holds it.
        let old_alive = shared.jit.jit_cache.get(name, "value", "()I", user_cid);
        assert!(old_alive.is_some(), "the old body was published");
        shared
            .classes
            .class_manager
            .write()
            .redefine_class(user_cid, ldc_class(name, 2), RedefineOptions::default())
            .expect("a same-shape redefinition succeeds");
        shared.jit.note_class_redefinition_of(user_cid, name, false);
        assert!(
            stashed_callee_predates_redefinition(
                &shared,
                user_cid.as_u32(),
                name,
                "value",
                "()I",
                old_point
            ),
            "the user loader's class was redefined after its body was compiled"
        );
        assert!(
            !stashed_callee_predates_redefinition(
                &shared,
                app_cid.as_u32(),
                name,
                "value",
                "()I",
                old_point
            ),
            "the application loader's class of that name never was"
        );
    }

    /// Round 12 wave 2 (lane rt, R12W1-3): this file's `class_was_redefined`
    /// memo records the verdict at its stamp, serves it while the stamp holds,
    /// and never serves a `false` past the class's own redefinition.
    #[test]
    fn the_redefined_class_memo_is_served_only_at_its_stamp() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let (Some(cid), Some(other_cid)) = (
            define(&shared, "r12w2rt/MemoClass"),
            define(&shared, "r12w2rt/MemoOther"),
        ) else {
            return;
        };
        redefine(&shared, other_cid, "r12w2rt/MemoOther");
        let stamp_now = || {
            (
                shared.vm_identity,
                shared.jit.jit_cache.redefine_epoch(),
                cratonvm_classloading::store_epochs(shared.jit.class_layout_domain)
                    .class_definition_epoch(),
            )
        };
        let raw = cid.as_u32();
        REDEFINED_CLASS_MEMO.with(|m| m.borrow_mut().1.clear());
        assert!(!class_was_redefined(&shared, cid));
        assert!(class_was_redefined(&shared, other_cid));
        if !redefined_class_memo_enabled() {
            return;
        }
        assert_eq!(
            REDEFINED_CLASS_MEMO.with(|m| m.borrow().1.get(&raw).copied()),
            Some(false),
            "the verdict is recorded"
        );
        // Served while the stamp holds: a planted `true` proves no lookup ran.
        // The class-definition epoch is shared with other tests' VMs, so the
        // check counts only when the stamp did not move across the call (the
        // epochs are monotonic).
        let before = stamp_now();
        REDEFINED_CLASS_MEMO.with(|m| {
            let mut m = m.borrow_mut();
            m.0 = before;
            m.1.insert(raw, true);
        });
        let served = class_was_redefined(&shared, cid);
        if stamp_now() == before {
            assert!(served, "served from the memo at its stamp");
        }
        // A `false` stamped before the class's own redefinition is not served.
        REDEFINED_CLASS_MEMO.with(|m| {
            let mut m = m.borrow_mut();
            m.0 = stamp_now();
            m.1.insert(raw, false);
        });
        redefine(&shared, cid, "r12w2rt/MemoClass");
        assert!(
            class_was_redefined(&shared, cid),
            "a stale `false` survived the class's own redefinition"
        );
        REDEFINED_CLASS_MEMO.with(|m| m.borrow_mut().1.clear());
    }

    /// The `jit_lambda_int_to_double` memo answers only at its own stamp and
    /// for its own receiver class, and stays bounded.
    #[test]
    fn the_lambda_get_memo_serves_only_its_stamp_and_receiver_class() {
        let key = (9001usize, 7u32);
        let stamp: LambdaGetStamp = (1, 2, 3);
        LAMBDA_GET_MEMO.with(|m| m.borrow_mut().clear());
        assert_eq!(lambda_get_memo_shape(key, stamp), None);
        lambda_get_memo_store(
            key,
            LambdaGetMemo {
                stamp,
                impl_class_name: Some(std::sync::Arc::from("r12rt/Impl")),
                route: None,
            },
        );
        assert_eq!(lambda_get_memo_shape(key, stamp), Some(true));
        assert_eq!(
            lambda_get_memo_shape(key, (2, 2, 3)),
            None,
            "another generation"
        );
        assert_eq!(
            lambda_get_memo_shape(key, (1, 3, 3)),
            None,
            "another definition"
        );
        assert_eq!(
            lambda_get_memo_shape(key, (1, 2, 4)),
            None,
            "another redefinition"
        );
        assert!(lambda_get_memo_route(key, stamp, ClassId::new(5)).is_none());
        lambda_get_memo_note_route(key, stamp, ClassId::new(5), LambdaGetRoute::Generic);
        assert!(matches!(
            lambda_get_memo_route(key, stamp, ClassId::new(5)),
            Some(LambdaGetRoute::Generic)
        ));
        assert!(
            lambda_get_memo_route(key, stamp, ClassId::new(6)).is_none(),
            "another receiver class"
        );
        // A route resolved at an older stamp does not land in a newer memo.
        lambda_get_memo_note_route(
            key,
            (0, 2, 3),
            ClassId::new(6),
            LambdaGetRoute::Direct(None),
        );
        assert!(lambda_get_memo_route(key, stamp, ClassId::new(6)).is_none());
        for i in 0..(LAMBDA_GET_MEMO_CAP as u32 + 5) {
            lambda_get_memo_store(
                (1, i),
                LambdaGetMemo {
                    stamp,
                    impl_class_name: None,
                    route: None,
                },
            );
        }
        assert!(LAMBDA_GET_MEMO.with(|m| m.borrow().len()) <= LAMBDA_GET_MEMO_CAP);
        LAMBDA_GET_MEMO.with(|m| m.borrow_mut().clear());
    }
}

/// Round 12 wave 2, lane osrfix: the `(int) -> double` peephole's direct call
/// into a compiled `get(I)D` is finished like a dispatch helper's compiled arm,
/// so a trapped `get`'s stashed frame is resumed here instead of reaching the
/// caller's sink
/// (r12w1-orch-osr-catch-around-lambda-peephole-throws-internalerror).
#[cfg(test)]
mod r12w2_osrfix_lambda_get_service_tests {
    use super::*;
    use crate::config::VmConfig;

    /// The text of the production function whose definition starts with `sig`.
    fn production_fn(src: &str, sig: &str) -> String {
        let start = src.find(sig);
        assert!(start.is_some(), "`{sig}` not found");
        let body = &src[start.unwrap_or(0)..];
        body[..body.find("\n}\n").map_or(body.len(), |e| e + 3)].to_string()
    }

    /// The service site dispatches on the receiver, roots only the receiver,
    /// claims a stash of the getter's own frame, and its resume twin carries
    /// the receiver's class as the loader context and nothing else new.
    #[test]
    fn the_direct_get_site_dispatches_on_the_receiver_and_pins_only_it() {
        let info = &LAMBDA_GET_DIRECT_INFO;
        assert!(invoke_site_has_receiver(info.invoke_kind));
        assert!(
            matches!(info.invoke_kind, 0 | 2),
            "resolved from the receiver's class"
        );
        assert_eq!(
            info.declaring_class_id, 0,
            "the route already asked selection"
        );
        let mut pinned = Vec::new();
        for_each_jit_reference_arg(info, &[0x1000, 7], |index, _| pinned.push(index));
        assert_eq!(pinned, vec![0], "the receiver, not the int index");
        assert!(exceptional_frame_key_names(
            "app/Thrower.get:(I)D",
            info.method_name,
            info.descriptor
        ));
        assert!(!exceptional_frame_key_names(
            "app/Loop.loop:(Ljava/util/function/Function;I)D",
            info.method_name,
            info.descriptor
        ));
        let site = lambda_get_resume_site(ClassId::new(77));
        assert_eq!(site.declaring_class_id, 77);
        assert_eq!(site.owner_class_id, 0);
        assert_eq!(
            (site.method_name, site.descriptor, site.invoke_kind),
            (info.method_name, info.descriptor, info.invoke_kind)
        );
        assert_eq!(
            (site.num_jit_args, site.return_type),
            (info.num_jit_args, info.return_type)
        );
    }

    /// A value, and a bare `i64::MIN` with no signal beside it (a `get` that
    /// returned `-0.0`), come back unchanged; no call is no answer.
    #[test]
    fn a_value_or_a_bare_negative_zero_passes_through_unchanged() {
        let vm = SharedVm::new(VmConfig::default());
        let _ = take_jit_deopt_pending();
        let args = [0i64, 3];
        // SAFETY: no JIT thread is installed on this test thread, so no
        // `jit_thread_mut` borrow is live, and `call` enters no compiled code.
        let (plain, neg_zero, none) = unsafe {
            (
                lambda_get_call_then_route(&vm, ClassId::new(0), &args, || Some(42)),
                lambda_get_call_then_route(&vm, ClassId::new(0), &args, || Some(i64::MIN)),
                lambda_get_call_then_route(&vm, ClassId::new(0), &args, || None),
            )
        };
        assert_eq!(plain, Some(42));
        assert_eq!(neg_zero, Some(i64::MIN));
        assert_eq!(none, None);
    }

    /// The peephole enters its compiled `get` only inside the service, and the
    /// service resumes a pure deopt before the general routing. A text pin:
    /// the helper cannot be driven without compiled code. Before the fix the
    /// body called the entry directly and returned its raw result.
    #[test]
    fn the_peephole_enters_compiled_get_only_under_the_service() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let body = production_fn(&src, "\nunsafe fn jit_lambda_int_to_double_body(");
        let service = body.find("lambda_get_call_then_route(");
        let entry = body.find("try_call_compiled_entry_reentrant(");
        assert!(
            matches!((service, entry), (Some(s), Some(e)) if s < e),
            "the compiled `get` is entered inside the service's closure"
        );
        assert_eq!(
            body.matches("try_call_compiled_entry_reentrant(").count(),
            1
        );
        let route = production_fn(&src, "\nunsafe fn lambda_get_call_then_route(");
        let resume = route.find("resume_trapped_lambda_get(");
        let general = route.find("route_implicit_exc_through_callee(");
        assert!(
            matches!((resume, general), (Some(r), Some(g)) if r < g),
            "a pure deopt is resumed before the general routing"
        );
    }
}

/// Interpreter round i1 wave 43, lane L2
/// (`interpreter-L2-a-spinning-obsolete-frame-sometimes-throws-internalerror-across-a-renumbering-redefinition-FIXED-20261007`):
/// a compiled site's index judged without the class-manager guard holds only
/// for the pool it was judged against. A site compiled at the current count is
/// not translated by the lock-free verdict, and a redefinition that begins
/// after that verdict must send the caller to the guarded judgement, which
/// translates it. On the host the helper read the NEW pool at the old index
/// (`JIT ldc: cp#19 ... holds Utf8("length")`).
#[cfg(test)]
mod i43_l2_cp_site_tests {
    use super::*;
    use cratonvm_classloading::{ClassLoaderId, DefineClassOptions, RedefineOptions};

    /// `i43l2/CpSite`: `static value()I` is `ldc #8; ireturn`, `#8` being
    /// `CONSTANT_Integer constant` (the `i20_l1_cp_site_index_tests` shape).
    fn ldc_class(constant: i32) -> Vec<u8> {
        fn utf8(out: &mut Vec<u8>, s: &str) {
            out.push(1);
            out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
        b.extend_from_slice(&9u16.to_be_bytes()); // constant_pool_count
        utf8(&mut b, "i43l2/CpSite"); // #1
        b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
        utf8(&mut b, "java/lang/Object"); // #3
        b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
        utf8(&mut b, "value"); // #5
        utf8(&mut b, "()I"); // #6
        utf8(&mut b, "Code"); // #7
        b.push(3); // #8 Integer
        b.extend_from_slice(&constant.to_be_bytes());
        b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
        b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
        b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
        b.extend_from_slice(&[0, 1]); // methods_count
        b.extend_from_slice(&[0x00, 0x09, 0, 5, 0, 6, 0, 1]); // public static value()I
        let code = [0x12, 8, 0xac]; // ldc #8; ireturn
        b.extend_from_slice(&[0, 7]); // "Code"
        b.extend_from_slice(&(12 + code.len() as u32).to_be_bytes());
        b.extend_from_slice(&[0, 1, 0, 0]); // max_stack 1, max_locals 0
        b.extend_from_slice(&(code.len() as u32).to_be_bytes());
        b.extend_from_slice(&code);
        b.extend_from_slice(&[0, 0, 0, 0]); // no handlers, no attributes
        b.extend_from_slice(&[0, 0]); // no class attributes
        b
    }

    /// The word compiled code bakes for code compiled at `stamp`.
    fn word(cid: ClassId, stamp: u64) -> i64 {
        // Cast: the packed word as the helper ABI's i64 bit pattern.
        (u64::from(cid.as_u32()) | ((stamp + 1) << 32)) as i64
    }

    #[test]
    fn a_verdict_taken_before_a_redefinition_is_judged_again_under_the_guard() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let cid = {
            let mut cm = shared.classes.class_manager.write();
            match cm.define_class_with_options(
                "i43l2/CpSite",
                &ldc_class(70_000),
                ClassLoaderId::Application,
                DefineClassOptions::default(),
            ) {
                Ok(cid) => cid,
                // The stripped test VM could not define it: nothing to run.
                Err(_) => return,
            }
        };
        let compiled_at = crate::classloading::class_redefinition_count();
        let site = CpSite::of(word(cid, compiled_at), 8);
        // The helper's lock-free verdict, taken before the redefinition
        // begins: the code is as new as the count, so nothing to translate.
        let (fast, judged_at) = site.index_lock_free(&shared);
        assert_eq!(fast, 8);
        shared
            .classes
            .class_manager
            .write()
            .redefine_class(cid, ldc_class(80_000), RedefineOptions::default())
            .expect("a same-shape redefinition succeeds");
        assert!(
            !CpSite::verdict_still_holds(judged_at),
            "a redefinition that began after the verdict voids it"
        );
        let cm = shared.classes.class_manager.read();
        let pool = &cm.get_class(cid).expect("loaded").constant_pool;
        let judged = site.index_in(&cm);
        assert_ne!(judged, 8, "#8 is the new constant now");
        assert!(matches!(
            pool.get(judged),
            Some(cratonvm_reader::constant_pool::ConstantPoolEntry::Integer(
                70_000
            ))
        ));
        let fresh = CpSite::of(
            word(cid, crate::classloading::class_redefinition_count()),
            8,
        );
        assert_eq!(fresh.index_in(&cm), 8, "code compiled after it reads #8");
        let unstamped = CpSite::of(i64::from(cid.as_u32()), 8);
        assert_eq!(
            unstamped.index_in(&cm),
            8,
            "a word with no stamp reads its index as it is"
        );
    }
}

/// JIT round 16 (lane mh, MH15-5): the constant-handle binding's own pieces.
#[cfg(test)]
mod r16_mh_constant_bind_tests {
    use super::*;
    use cratonvm_native_builtins::lang_invoke::MhCompiledBind;

    /// A VM identity no other test uses (the interning table is per thread).
    const VM: usize = 0x16C0_0001;

    fn bind(class: &str, name: &str, desc: &str, kind: u8) -> MhCompiledBind {
        MhCompiledBind {
            invoke_kind: kind,
            class_id: 0x16C1,
            class: class.to_string(),
            name: name.to_string(),
            desc: desc.to_string(),
            bound: false,
        }
    }

    /// One interned info per (class, name, descriptor, kind), with the slot
    /// count and return tag a compiled site of that call would carry.
    #[test]
    fn a_bound_target_info_is_interned_and_shaped_as_a_compiled_site() {
        let st = mh_bind_leaf_info(VM, &bind("p/C", "m", "(IJ)D", 3)).expect("static info");
        let again = mh_bind_leaf_info(VM, &bind("p/C", "m", "(IJ)D", 3)).expect("static info");
        assert!(std::ptr::eq(st, again), "interned");
        assert_eq!(
            (st.num_jit_args, st.return_type, st.invoke_kind),
            (2, b'D', 3)
        );
        // The target's own class is the call's class context (the callee-resume
        // fix, `r16-mh-constant-bind-callee-resume-refused-FIXED-20261001.md`).
        assert_eq!((st.declaring_class_id, st.owner_class_id), (0x16C1, 0));
        // Another class id (another loader's copy of `p/C`) or another VM is
        // another info.
        let mut other = bind("p/C", "m", "(IJ)D", 3);
        other.class_id = 0x16C2;
        let o = mh_bind_leaf_info(VM, &other).expect("other class");
        assert!(!std::ptr::eq(st, o));
        assert_eq!(o.declaring_class_id, 0x16C2);
        let v2 = mh_bind_leaf_info(VM + 1, &bind("p/C", "m", "(IJ)D", 3)).expect("other vm");
        assert!(!std::ptr::eq(st, v2));
        let vi = mh_bind_leaf_info(VM, &bind("p/C", "m", "(IJ)D", 0)).expect("virtual info");
        assert!(!std::ptr::eq(st, vi), "the kind is part of the key");
        assert_eq!(vi.num_jit_args, 3, "the receiver is a slot");
        let r = mh_bind_leaf_info(VM, &bind("p/C", "r", "()Lp/C;", 3)).expect("ref info");
        assert_eq!(r.return_type, b'L');
    }

    /// Round 16 wave 8 (lane mh): a bound (`bindTo`) virtual site passes as
    /// many words as its target takes -- the handle's word becomes the
    /// receiver's -- which `mh_constant_bind_redirect` checks before
    /// `mh_constant_bind_call` splices; the receiver is the `MH_BOUND` guard word.
    #[test]
    fn a_bound_virtual_target_takes_the_sites_word_count() {
        let mut b = bind("p/C", "v", "(IJ)I", 0);
        b.bound = true;
        let info = mh_bind_leaf_info(VM, &b).expect("virtual info");
        // The site `(IJ)I` on the handle: the handle and two values.
        assert_eq!(info.num_jit_args, 1 + 2);
        assert!(info.num_jit_args <= MH_BIND_MAX_SPLICED_ARGS);
        // An object's guard word is its address, so it can be spliced as is.
        let addr = 0x7_0000_0040usize;
        let obj = unsafe { ObjectRef::from_raw(addr as *mut u8) };
        assert_eq!(mh_bind_slot_key(Value::Object(Some(obj))), addr as u64);
    }

    /// Guard words: an `Int` never equals a reference or null.
    #[test]
    fn slot_keys_keep_ints_apart_from_references() {
        assert_eq!(mh_bind_slot_key(Value::Object(None)), 0);
        assert_ne!(mh_bind_slot_key(Value::Int(0)), 0);
        assert_ne!(mh_bind_slot_key(Value::Int(8)), 8);
        assert_ne!(
            mh_bind_slot_key(Value::Int(1)),
            mh_bind_slot_key(Value::Int(2))
        );
    }

    /// Round 16 wave 10 (lane mhfloor, MH16-8): the direct entry takes an
    /// ordinary bound target and leaves to the dispatch ladder every target a
    /// name-decided ladder arm may answer before its compiled-entry memos.
    #[test]
    fn the_direct_entry_leaves_name_decided_arms_to_the_ladder() {
        let st = mh_bind_leaf_info(VM, &bind("p/D", "m", "(I)I", 3)).expect("static info");
        let vi = mh_bind_leaf_info(VM, &bind("p/D", "v", "(I)I", 0)).expect("virtual info");
        assert!(
            mh_bind_direct_leaf_eligible(st),
            "an ordinary static target"
        );
        assert!(
            mh_bind_direct_leaf_eligible(vi),
            "an ordinary virtual target"
        );
        let integer = mh_bind_leaf_info(
            VM,
            &bind("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;", 3),
        )
        .expect("integer info");
        assert!(
            !mh_bind_direct_leaf_eligible(integer),
            "the boxing cache's site"
        );
        for name in ["getResource", "getResources", "getResourceAsStream"] {
            let r = mh_bind_leaf_info(
                VM,
                &bind("p/L", name, "(Ljava/lang/String;)Ljava/lang/Object;", 0),
            )
            .expect("resource info");
            assert!(!mh_bind_direct_leaf_eligible(r), "the null-name NPE arm");
        }
        // JIT round 19 wave 1 (lane mhiface): an interface target is eligible;
        // the fill decides whether its site goes direct
        // (`CRATONVM_JIT_MH_CONSTANT_BIND_DIRECT_INTERFACE`).
        let iface = mh_bind_leaf_info(VM, &bind("p/D", "i", "(I)I", 2)).expect("interface info");
        assert!(
            mh_bind_direct_leaf_eligible(iface),
            "an interface target behind the redirect's receiver guard"
        );
        let special = mh_bind_leaf_info(VM, &bind("p/D", "s", "(I)I", 4)).expect("kind 4 info");
        assert!(
            !mh_bind_direct_leaf_eligible(special),
            "a kind the bind does not mint"
        );
    }

    /// The switch is opt-in and read by the fill, not the warm path.
    #[test]
    fn the_direct_entry_switch_is_default_on_with_a_kill_switch() {
        const SWITCH: &str = "CRATONVM_JIT_MH_CONSTANT_BIND_DIRECT";
        let default = cratonvm_types::flags::with_thread_overrides(&[(SWITCH, None)], || {
            mh_constant_bind_direct_flag()
        });
        assert!(default, "default ON since the w10a A/B");
        let off = cratonvm_types::flags::with_thread_overrides(&[(SWITCH, Some("0"))], || {
            mh_constant_bind_direct_flag()
        });
        assert!(!off, "`0` turns it off");
        let on = cratonvm_types::flags::with_thread_overrides(&[(SWITCH, Some("1"))], || {
            mh_constant_bind_direct_flag()
        });
        assert!(on, "`1` turns it on");
    }

    /// Round 16 wave 11 (lane mhbind, MHF-1): the shape-keyed bind is opt-in
    /// and read by the fill, not the warm path.
    #[test]
    fn the_shape_bind_switch_is_default_on() {
        const SWITCH: &str = "CRATONVM_JIT_MH_CONSTANT_BIND_SHAPE";
        let default = cratonvm_types::flags::with_thread_overrides(&[(SWITCH, None)], || {
            mh_constant_bind_shape_flag()
        });
        assert!(
            default,
            "default ON since the w11d A/B (bind-each 3480 -> 1794 ns with the fast mint)"
        );
        let off = cratonvm_types::flags::with_thread_overrides(&[(SWITCH, Some("0"))], || {
            mh_constant_bind_shape_flag()
        });
        assert!(!off, "`0` turns it off");
        let on = cratonvm_types::flags::with_thread_overrides(&[(SWITCH, Some("1"))], || {
            mh_constant_bind_shape_flag()
        });
        assert!(on, "`1` turns it on");
    }

    /// MHF-1: the shape and the guard together name every synthetic slot of
    /// a bound handle once, and the shape's words are distinct from the
    /// guard's (a shape match re-reads no guard slot as a shape slot).
    #[test]
    fn the_shape_slots_extend_the_guard_slots() {
        use cratonvm_native_builtins::lang_invoke::{
            MH_COMPILED_BIND_GUARD_SLOTS, MH_COMPILED_BIND_SHAPE_SLOTS,
        };
        for slot in MH_COMPILED_BIND_SHAPE_SLOTS {
            assert!(!MH_COMPILED_BIND_GUARD_SLOTS.contains(&slot));
        }
        let shape = MhBindShape {
            class_id: 7,
            width: 26,
            type_slot: 0,
            words: [0; 5],
        };
        assert_eq!(shape.words.len(), MH_COMPILED_BIND_SHAPE_SLOTS.len() + 1);
    }

    /// Round 16 wave 15 (lane mh6; MHB-1): the converting bind is opt-in and
    /// read by the fill, not the warm path; so is its debug line.
    #[test]
    fn the_converting_bind_switch_is_default_on() {
        const SWITCH: &str = "CRATONVM_JIT_MH_CONSTANT_BIND_CONVERTING";
        let default = cratonvm_types::flags::with_thread_overrides(&[(SWITCH, None)], || {
            mh_constant_bind_converting_flag()
        });
        assert!(default, "default ON since wave 17 (w15a A/B)");
        let off = cratonvm_types::flags::with_thread_overrides(&[(SWITCH, Some("0"))], || {
            mh_constant_bind_converting_flag()
        });
        assert!(!off, "`0` turns it off");
        const DBG: &str = "CRATONVM_DBG_JIT_MH_BIND";
        assert!(!cratonvm_types::flags::with_thread_overrides(
            &[(DBG, None)],
            mh_constant_bind_dbg
        ));
        assert!(cratonvm_types::flags::with_thread_overrides(
            &[(DBG, Some("1"))],
            mh_constant_bind_dbg
        ));
    }

    fn conv(unbox_int: u32, box_int_return: bool) -> MhBindConv {
        MhBindConv {
            type_slot: 0,
            type_word: 0x7_0000_0100,
            unbox_int,
            box_int_return,
            integer_class_id: 0x16C3,
            value_slot: 0,
        }
    }

    /// Round 16 wave 15 (lane mh6; MHB-1): a converting call's member words
    /// are the site's words after the handle, unboxed exactly where the
    /// conversion says; a word that will not unbox, or a call wider than the
    /// buffer, is the door's.
    #[test]
    fn a_converting_call_unboxes_only_its_marked_words() {
        let boxed = 0x7_0000_0040i64;
        let unbox = |w: i64| (w == boxed).then_some(-42i64);
        let out = mh_bind_convert_args(&conv(0b10, true), &[5, boxed, boxed], unbox)
            .expect("the marked word unboxes");
        assert_eq!(&out.words[..3], &[5, -42, boxed], "word 2 passes through");
        assert!(out.box_int_return);
        // A null, or any word the unbox refuses, sends the call to the door.
        assert!(mh_bind_convert_args(&conv(0b1, false), &[0], unbox).is_none());
        assert!(mh_bind_convert_args(&conv(0b1, false), &[0x7_0000_0080], unbox).is_none());
        // Nothing marked: the words as they are.
        let plain = mh_bind_convert_args(&conv(0, true), &[1, 2], unbox).expect("plain");
        assert_eq!(&plain.words[..2], &[1, 2]);
        // Wider than the buffer.
        let wide = [boxed; MH_BIND_MAX_SPLICED_ARGS + 1];
        assert!(mh_bind_convert_args(&conv(0b1, false), &wide, unbox).is_none());
    }

    /// Round 16 wave 15 (lane mh6; MHB-1): the leaf of a converting bind is
    /// the member's own info -- the one an exact bind of the member interns --
    /// so a converting site's words match its slot count, and its dispatch
    /// memos are the member's.
    #[test]
    fn a_converting_bind_calls_the_members_own_info() {
        let st = mh_bind_leaf_info(VM, &bind("p/E", "twice", "(I)I", 3)).expect("static info");
        // The site `(Object)Object` on the handle: the handle and one value.
        assert_eq!(st.num_jit_args + 1, 2);
        assert_eq!(st.return_type, b'I', "boxed by the helper, not the leaf");
        let again = mh_bind_leaf_info(VM, &bind("p/E", "twice", "(I)I", 3)).expect("again");
        assert!(std::ptr::eq(st, again));
    }
}

#[cfg(all(test, feature = "zgc"))]
mod zgc_store_plan_tests {
    use super::*;
    use crate::memory::vm_heap::VmHeap;
    use cratonvm_gc::zgc::ZgcRealHeap;

    #[test]
    fn helper_selection_uses_the_owning_zgc_gate_block() {
        let a = VmHeap::Zgc(std::sync::Arc::new(ZgcRealHeap::with_capacity(
            8 * 1024 * 1024,
        )));
        let b = VmHeap::Zgc(std::sync::Arc::new(ZgcRealHeap::with_capacity(
            8 * 1024 * 1024,
        )));
        let pa = ref_store_plan_for_heap(Some(&a));
        let pb = ref_store_plan_for_heap(Some(&b));
        assert_ne!(pa.0 .0, pb.0 .0);
        assert_ne!(pa.0 .1, pb.0 .1);
        if let VmHeap::Zgc(h) = &a {
            let own = h.jit_ref_store_plan();
            assert_eq!(pa, ((own.0, own.1, own.2), 0));
        }
        if let VmHeap::Zgc(h) = &b {
            h.set_mark_active(false);
            h.set_generational_enabled(false);
        }
        drop(b);
        assert_eq!(ref_store_plan_for_heap(Some(&a)), pa);
    }
}

#[cfg(test)]
mod r16w12_throw3_callee_verdict_identity {
    //! JIT round 16 wave 12 (lane throw3, TP16-14): the callee exception-table
    //! verdict answered by the info's identity (the retire epoch it was proved
    //! at) before its content.

    use super::{CalleeTableVerdict, JitInvokeInfo, INTEGER_INT_VALUE_INFO, INTEGER_VALUE_OF_INFO};

    const VM: usize = 7;
    const STAMP: (u64, u64) = (3, 0);

    fn verdict_for(info: &JitInvokeInfo, retire_epoch: u64) -> CalleeTableVerdict {
        CalleeTableVerdict {
            vm_identity: VM,
            selected: false,
            receiver: 0,
            declaring: info.declaring_class_id,
            owner: info.owner_class_id,
            kind: info.invoke_kind,
            class_name: Box::from(info.class_name),
            method_name: Box::from(info.method_name),
            descriptor: Box::from(info.descriptor),
            stamp: STAMP,
            has_table: true,
            retire_epoch,
        }
    }

    #[test]
    fn switch_off_compares_the_names_as_before() {
        let v = verdict_for(&INTEGER_VALUE_OF_INFO, 5);
        assert_eq!(
            v.answer(VM, &INTEGER_VALUE_OF_INFO, false, 0, STAMP, None),
            (Some(true), false)
        );
        let (other, _) = v.answer(VM, &INTEGER_INT_VALUE_INFO, false, 0, STAMP, None);
        assert_eq!(other, None, "another site's names miss");
    }

    #[test]
    fn at_the_proved_epoch_the_scalars_decide() {
        let v = verdict_for(&INTEGER_VALUE_OF_INFO, 5);
        assert_eq!(
            v.answer(VM, &INTEGER_VALUE_OF_INFO, false, 0, STAMP, Some(5)),
            (Some(true), false)
        );
        // Every integer field still counts.
        let info = &INTEGER_VALUE_OF_INFO;
        let at5 = |vm: usize, selected: bool, receiver: u32, stamp: (u64, u64)| {
            v.answer(vm, info, selected, receiver, stamp, Some(5)).0
        };
        assert_eq!(at5(VM, false, 0, (4, 0)), None);
        assert_eq!(at5(VM + 1, false, 0, STAMP), None);
        assert_eq!(at5(VM, true, 0, STAMP), None);
        assert_eq!(at5(VM, false, 9, STAMP), None);
    }

    /// After a retire the address may name another info: the names decide,
    /// and a match asks the caller to re-stamp the entry.
    #[test]
    fn after_a_retire_the_names_decide_and_a_match_refreshes() {
        let v = verdict_for(&INTEGER_VALUE_OF_INFO, 5);
        assert_eq!(
            v.answer(VM, &INTEGER_VALUE_OF_INFO, false, 0, STAMP, Some(6)),
            (Some(true), true)
        );
        assert_eq!(
            v.answer(VM, &INTEGER_INT_VALUE_INFO, false, 0, STAMP, Some(6)),
            (None, false)
        );
    }
}

#[cfg(test)]
mod gcj3rt3_mh_site_identity_tests {
    //! gcj j3/rt3 (`gcj-j2jt2-mh-site-memos-keyed-by-a-reusable-jit-invoke-info-address`):
    //! the compiled `MethodHandle` site rows answer only for the site that
    //! made them, after a retire re-issued their info address.

    use super::{JitInvokeInfo, MhSiteIdentity};

    fn mh_site(method: &'static str, descriptor: &'static str, declaring: u32) -> JitInvokeInfo {
        JitInvokeInfo {
            class_name: "java/lang/invoke/MethodHandle",
            method_name: method,
            descriptor,
            num_jit_args: 2,
            return_type: 0,
            invoke_kind: 0,
            declaring_class_id: declaring,
            owner_class_id: 0,
        }
    }

    #[test]
    fn gcj3rt3_an_equal_retire_epoch_names_the_site_without_reading_it() {
        let a = mh_site("invokeExact", "(LA;)V", 40);
        let row = MhSiteIdentity::at(&a, 5);
        // Another site at the same address cannot exist at epoch 5: the epoch
        // alone answers, and the switch is never asked.
        let b = mh_site("invokeExact", "(LB;)V", 41);
        assert!(row.names(&b, 5, || panic!("the switch is read only on a mismatch")));
    }

    #[test]
    fn gcj3rt3_after_a_retire_only_the_same_content_is_the_same_site() {
        let a = mh_site("invokeExact", "(LA;)V", 40);
        let row = MhSiteIdentity::at(&a, 5);
        assert!(!row.names(&mh_site("invokeExact", "(LB;)V", 40), 6, || true));
        assert!(!row.names(&mh_site("invokeExact", "(LA;)V", 41), 6, || true));
        assert!(!row.names(&mh_site("invoke", "(LA;)V", 40), 6, || true));
        assert_eq!(row.retire_epoch.get(), 5, "a miss does not re-stamp");
        assert!(row.names(&a, 6, || true));
        assert_eq!(row.retire_epoch.get(), 6, "a content match re-stamps");
        assert!(row.names(&mh_site("invokeExact", "(LB;)V", 41), 6, || panic!()));
    }

    #[test]
    fn gcj3rt3_the_kill_switch_answers_as_the_address_only_rows() {
        let row = MhSiteIdentity::at(&mh_site("invokeExact", "(LA;)V", 40), 5);
        assert!(row.names(&mh_site("invoke", "(LB;)V", 41), 6, || false));
        assert_eq!(row.retire_epoch.get(), 5);
    }

    /// Source pins: both rows are proved before they are believed.
    #[test]
    fn gcj3rt3_both_mh_rows_check_identity_before_use() {
        let src = crate::jit::helpers::HELPERS_SRC;
        // The needles are split so this test's own text never matches them.
        let after = |anchor: &str, needle: &str| -> usize {
            let at = src.find(anchor).expect("anchor");
            src[at..].find(needle).expect("needle")
        };
        let warm = concat!("let warm = MH_BIND", "_SITES.with(|sites| {");
        let proof = after(warm, concat!("site.identity", ".names(info, retire_epoch"));
        let seen = after(warm, concat!("site_seen", " = true;"));
        assert!(proof < seen, "a row is seen only after it is proved");
        assert!(seen < 600, "both inside the warm path's closure");
        let linked = concat!("let linked = MH_LINKED", "_SITES.with(|s| {");
        let link_proof = concat!("row.names(info, retire_epoch, ", "mh_site_identity_enabled)");
        assert!(after(linked, link_proof) < 200);
        // JIT round 21 wave 19 (lane mhbind2) added the real-shape memo to the tuple.
        let fill = concat!(
            "let (prev_fills, prev_epoch, prev_real_shape) = MH_BIND",
            "_SITES.with("
        );
        assert!(after(fill, concat!(".filter(|s| s.identity", ".names(")) < 200);
    }
}

#[cfg(test)]
mod byte_array_view_read_abi_tests {
    use super::*;
    #[test]
    fn view_helper_addresses_and_fallback_shapes_agree() {
        let fns = varhandle_byte_array_view_read_direct_fns();
        let unique: std::collections::HashSet<_> = fns.iter().copied().collect();
        assert_eq!(unique.len(), 6);
        for (slot, info) in VH_BYTE_ARRAY_VIEW_READ_INFOS.iter().enumerate() {
            assert_eq!(info.num_jit_args, 3);
            assert_eq!(
                cratonvm_jit::varhandle_byte_array_view_read_slot(
                    info.method_name,
                    info.descriptor
                ),
                Some(slot)
            );
            assert_eq!(
                info.return_type,
                cratonvm_jit::VARHANDLE_BYTE_ARRAY_VIEW_RETURNS[slot]
            );
            assert_eq!(varhandle_byte_array_view_read_direct_fn(slot), fns[slot]);
        }
        let sig = cratonvm_jit::direct_helpers::direct_helper_sig("varhandle_byte_array_view_read")
            .unwrap();
        assert_eq!(sig.arity, 4);
    }
}

/// Source witness for the JVMS 6.5 receiver test of the blind dispatch door
/// (`R16ThrowpathPlain npeCall`, page
/// `r18-merged-tree-reuses-freed-jit-code-under-the-warm-only-eager-chain-20261006.md`):
/// a null receiver of a statically bound instance site must be refused BEFORE
/// the first arm that CALLs a compiled callee entry.
#[cfg(test)]
mod r18_dispatch_null_receiver_witness {
    #[test]
    fn a_null_special_receiver_is_refused_before_any_compiled_callee_arm() {
        let src = crate::jit::helpers::HELPERS_SRC;
        let start = src
            .find("\nunsafe fn jit_invoke_dispatch_body(")
            .expect("jit_invoke_dispatch_body");
        let body = &src[start..];
        let check = body
            .find("if info.invoke_kind == 1 && args_slice.first() == Some(&0) {")
            .expect("the kind-1 null-receiver test is gone from the dispatch door");
        let first_compiled_arm = body
            .find("DISPATCH_CACHE.with(")
            .expect("the dispatch cache arm");
        assert!(
            check < first_compiled_arm,
            "the kind-1 null-receiver test must precede every compiled-callee arm"
        );
    }
}

/// nio20 wave 1, lane L3 (NIO19-5): the compiled-`getClass()` word table is
/// per VM, published to the optimizing tier through `direct_helper_table_for`,
/// and filled by the doors that already answer `getClass()` from
/// `class_mirrors`.
#[cfg(test)]
mod nio20_getclass_mirror_words_tests {
    use super::*;
    use crate::config::VmConfig;

    #[test]
    fn the_compile_table_names_this_vms_words_and_a_served_class_publishes_its_mirror() {
        if !cfg!(target_arch = "x86_64") || !cratonvm_jit::getclass_inline_enabled() {
            // `CRATONVM_JIT_GETCLASS_INLINE=0`, or a backend with no reader:
            // no table is published, which is the other arm.
            return;
        }
        let shared = SharedVm::new(VmConfig::default());
        let table = direct_helper_table_for(&shared);
        assert_ne!(table.getclass_mirror_words, 0);
        assert_eq!(
            table.getclass_mirror_words,
            shared.classes.getclass_mirror_words.base_addr(),
            "the compile bakes THIS VM's table"
        );
        let other = SharedVm::new(VmConfig::default());
        assert_ne!(
            direct_helper_table_for(&other).getclass_mirror_words,
            table.getclass_mirror_words,
            "per VM, never a process global"
        );

        let cid = ClassId::new(62_001);
        let mirror = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        shared.classes.class_mirrors.write().insert(cid, mirror);
        assert_eq!(
            stamped_class_mirror_publishing_word(&shared, cid),
            Some(mirror)
        );
        // Cast: the mirror's address, as compiled code reads the word.
        assert_eq!(
            shared.classes.getclass_mirror_words.get(cid),
            mirror.as_ptr() as usize
        );

        // No mirror: nothing to answer, nothing published.
        let absent = ClassId::new(62_002);
        assert_eq!(stamped_class_mirror_publishing_word(&shared, absent), None);
        assert_eq!(shared.classes.getclass_mirror_words.get(absent), 0);
    }
}

/// `r16-ffm-plain-downcall-costs-microseconds` (FFM217-2): the FFM direct
/// door. Text pins, since the door cannot be driven without compiled code.
#[cfg(test)]
mod ffm_direct_door_tests {
    fn body_of(src: &str, sig: &str) -> String {
        let start = src.find(sig).expect("the function exists");
        let rest = &src[start + 1..];
        rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)].to_string()
    }

    /// The door is the first thing the virtual-MIC helper asks, before the
    /// lean native door and the panic scope.
    #[test]
    fn the_door_is_asked_first_in_the_virtual_mic_helper() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let body = body_of(&src, "\npub unsafe extern \"C\" fn jit_invoke_virtual_mic(");
        let door = body
            .find("try_ffm_direct_door(")
            .expect("the door is asked");
        let lean = body
            .find("try_lean_native_door(")
            .expect("the lean door is asked");
        assert!(door < lean);
    }

    /// A hit needs the bind's own identity test (the same handle address in
    /// the same collection epoch) and every generation the fill saw, before
    /// anything runs; the call itself is the bound row's
    /// `ffm_downcall_direct_call`, which re-asks the per-call admission.
    #[test]
    fn a_hit_is_the_bound_handle_in_the_fill_epoch() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let body = body_of(&src, "\nunsafe fn try_ffm_direct_door(");
        let call = body
            .find("ffm_downcall_direct_call_with(")
            .expect("the bound row's call");
        for guard in [
            "args[0] as u64 != site.handle",
            "site.epoch != vm.mem.address_epoch()",
            "site.jit_gen != cratonvm_jit::jit_cache_generation()",
            "site.registry_gen != vm.natives.native_methods.generation()",
            "any_class_redefined()",
            "jvmti_requires_interpreter(vm)",
            "check_native_dispatch_capability_for_slot",
        ] {
            let at = body
                .find(guard)
                .unwrap_or_else(|| panic!("missing guard {guard}"));
            assert!(at < call, "{guard} runs before the call");
        }
        // The arguments are forwarded before the handle is compared and before
        // the call reads any of them (a relocation in progress moves them
        // without moving the epoch).
        let forward = body
            .find("forward_jit_reference_args(vm, info, args)")
            .expect("the arguments are forwarded");
        let compare = body
            .rfind("args[0] as u64 != site.handle")
            .expect("the forwarded compare");
        assert!(forward < compare && compare < call);
        // The fill records the epoch read before the C call.
        let bound = body_of(&src, "\nunsafe fn try_jit_ffm_downcall_bound(");
        let epoch = bound
            .find("let epoch = vm.mem.address_epoch();")
            .expect("epoch");
        let call = bound.find("ffm_downcall_direct_call(").expect("call");
        let fill = bound.find("ffm_direct_door_fill(").expect("fill");
        assert!(epoch < call && call < fill);
    }

    #[test]
    fn every_slot_index_is_in_range() {
        for addr in [0usize, 8, 0x1234_5670, usize::MAX] {
            assert!(super::ffm_direct_door_slot(addr) < super::FFM_DIRECT_DOOR_SLOTS);
        }
    }

    /// JIT round 22 (FFM22-1 step (b)): the thin helper widens the staged
    /// arguments and reads `rax` by the row's codes exactly as the compiled
    /// call's own `MOVSX` / `MOVZX` bytes do.
    #[test]
    fn the_lazy_helper_widens_by_the_row_codes() {
        use cratonvm_jit::ir_lower as jl;
        assert_eq!(super::ffm_const_widen(0x1FF, jl::FFM_CONST_S8), u64::MAX);
        assert_eq!(super::ffm_const_widen(0x7F, jl::FFM_CONST_S8), 0x7F);
        assert_eq!(
            super::ffm_const_widen(0x1_FFFF, jl::FFM_CONST_S16),
            u64::MAX
        );
        assert_eq!(super::ffm_const_widen(0x1_FFFF, jl::FFM_CONST_U16), 0xFFFF);
        assert_eq!(
            super::ffm_const_widen(0x1_8000_0000, jl::FFM_CONST_S32),
            0xFFFF_FFFF_8000_0000
        );
        assert_eq!(
            super::ffm_const_widen(u64::MAX - 1, jl::FFM_CONST_W64),
            u64::MAX - 1
        );
        assert_eq!(super::ffm_const_widen(42, jl::FFM_CONST_VOID), 0);
    }
}

#[cfg(test)]
mod r19_throws19_local_handler_lean_tests {
    //! JIT round 19 proposal wave (lane throws19, TP16-15;
    //! `CRATONVM_JIT_LOCAL_HANDLER_LEAN=1`): the lean arm of
    //! [`jit_local_handler_lookup`] answers exactly as the default arm -- the
    //! same candidate, the same throwable handed to the handler slot, the same
    //! site cache -- on a miss, a hit, a miss of another class, and a re-miss.
    use super::*;
    use crate::threading::ThreadId;
    use cratonvm_jit::JitLocalHandlerSite;

    /// Restores the JIT thread pointer even if an assertion panics while it
    /// is installed.
    struct Installed(Option<JitThreadScope>);
    impl Drop for Installed {
        fn drop(&mut self) {
            if let Some(scope) = self.0.take() {
                restore_jit_thread(scope);
            }
        }
    }

    fn site(candidates: Vec<(&'static str, u32)>, declaring: ClassId) -> JitLocalHandlerSite {
        JitLocalHandlerSite {
            candidates,
            declaring_class_id: declaring.as_u32(),
            throw_bci: 0,
            cache: std::sync::atomic::AtomicU64::new(JitLocalHandlerSite::CACHE_EMPTY),
            implicit_cache: JitLocalHandlerSite::empty_implicit_cache(),
            no_collect: false,
        }
    }

    /// One compiled catch of `exc` at `site`: `(answer, handler slot, the
    /// pending throwable was taken)`.
    fn catch_once(
        shared: &SharedVm,
        site: &JitLocalHandlerSite,
        exc: ObjectRef,
    ) -> (i64, i64, bool) {
        let mut thread = Box::new(JvmThread::new(ThreadId(0), "r19-local-handler-lean"));
        thread.jit_pending_exception = Some(exc);
        let installed = Installed(Some(set_jit_thread(&mut thread)));
        let mut out = 0i64;
        // SAFETY: `shared` and `site` outlive the call, `out` is a live local
        // standing in for the handler's frame slot, and the JIT thread pointer
        // is installed for this OS thread -- the contract the local-handler
        // stub satisfies.
        let answer = unsafe {
            jit_local_handler_lookup_body(
                shared as *const SharedVm as i64,
                site as *const JitLocalHandlerSite as i64,
                &mut out,
                &std::cell::Cell::new(DeferredTrapSnapshot::None),
            )
        };
        drop(installed);
        (answer, out, thread.jit_pending_exception.is_none())
    }

    #[test]
    fn the_lean_arm_answers_as_the_default_arm() {
        const EXC: &str = "cratonvm/test/R19LeanHandlerExc";
        const OTHER: &str = "cratonvm/test/R19LeanHandlerOther";
        for lean in [false, true] {
            let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
            shared
                .threads
                .throwable_layout_cache
                .latch_local_handler_lean_for_test(lean);
            let (exc_class, other_class) = {
                let mut cm = shared.classes.class_manager.write();
                (
                    cm.try_ensure_synthetic_class(EXC, 0)
                        .expect("Compatible mode fabricates"),
                    cm.try_ensure_synthetic_class(OTHER, 0)
                        .expect("Compatible mode fabricates"),
                )
            };
            let exc = shared.mem.heap.alloc_object(exc_class, 0);
            let other = shared.mem.heap.alloc_object(other_class, 0);
            let site = site(vec![(EXC, 10), ("", 20)], exc_class);
            let cache = || {
                JitLocalHandlerSite::decode_cache(
                    site.cache.load(std::sync::atomic::Ordering::Relaxed),
                )
            };
            // A miss on an empty cache, then a hit on the same class.
            for round in 0..2 {
                let (answer, slot, taken) = catch_once(&shared, &site, exc);
                assert_eq!(answer, 0, "lean={lean} round={round}: the typed row");
                assert_eq!(
                    slot,
                    exc.as_ptr() as i64,
                    "lean={lean}: the throwable handed over"
                );
                assert!(taken, "lean={lean}: the pending throwable is taken");
                assert_eq!(cache(), Some((exc_class.as_u32(), 0)));
            }
            // Another class misses the cache (the validated read) and takes
            // the catch-all; the cache follows it.
            let (answer, slot, taken) = catch_once(&shared, &site, other);
            assert_eq!(answer, 1, "lean={lean}: the catch-all row");
            assert_eq!(slot, other.as_ptr() as i64);
            assert!(taken);
            assert_eq!(cache(), Some((other_class.as_u32(), 1)));
            // And the first class misses back.
            let (answer, _, _) = catch_once(&shared, &site, exc);
            assert_eq!(
                answer, 0,
                "lean={lean}: a re-miss resolves the typed row again"
            );
            assert_eq!(cache(), Some((exc_class.as_u32(), 0)));
        }
    }
}

/// nio21 wave 18, lane L2: the cast thunk's installer. Text pins, since the
/// install cannot be driven without a compiled caller.
#[cfg(test)]
mod lambda_cast_thunk_install_tests {
    fn body_of(src: &str, sig: &str) -> String {
        let start = src.find(sig).expect("the function exists");
        let rest = &src[start + 1..];
        rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)].to_string()
    }

    /// A cast site installs only with every guard proved (asked BEFORE the
    /// per-site latch is claimed, so a call whose arguments had not proved a
    /// class yet does not spend the claim), with a MIC to bake, and under its
    /// switch (read once per claim).
    #[test]
    fn a_cast_site_is_guarded_before_the_claim() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let body = body_of(&src, "\nunsafe fn install_lambda_inline_cache(");
        let guards = body
            .find("site.cast_thunk_guards()")
            .expect("the guards are asked");
        let switch = body
            .find("\"CRATONVM_JIT_LAMBDA_CAST_THUNK\"")
            .expect("the kill switch is read");
        let claim = body.find("site.claim_adapter_install(").expect("the latch");
        // nio21 w19 L2: the switch is read AFTER the claim, so a site refused
        // for good does not read it on every call (2.4 M reads on w18 `hdr`).
        assert!(guards < claim && claim < switch);
        assert!(body.contains("mic_ptr == 0"), "no MIC, no miss edge");
        assert!(body.contains("lambda_cast_adapter_entry("));
    }

    /// The miss edge names THIS site, so a cast thunk never reaches the
    /// shared megamorphic table, where other sites would take it.
    #[test]
    fn a_cast_thunk_is_never_published_to_the_shared_table() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let body = body_of(&src, "\nunsafe fn install_lambda_inline_cache(");
        let publish = body
            .find("install_mega_dispatch_way(")
            .expect("the shared publish");
        let gate = body[..publish].rfind("cast_guards.is_none()");
        assert!(
            gate.is_some(),
            "the shared publish is gated on a plain thunk"
        );
        assert!(body.contains("helper: jit_invoke_virtual_mic as *const () as usize"));
    }

    /// nio21 w19 L2: the MIC tail asks for the selected method's body under
    /// its DECLARING class before the interpreted template and the by-name
    /// tail, only when the receiver-keyed probe found none, and the arm
    /// refuses wrapped / indy-trapping bodies, checks native shadows through
    /// the template build, and publishes into the per-thread entry cache.
    #[test]
    fn the_mic_tail_asks_for_the_declaring_class_body_first() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let body = body_of(&src, "\nunsafe fn jit_invoke_virtual_mic_body(");
        let arm = body
            .find("mic_tail_call_selected_declaring_body(")
            .expect("the arm is asked");
        let template = body
            .find("try_jit_virtual_bytecode_callee(")
            .expect("the interpreted template");
        let tail = body.find("\"jit_mic_tail\"").expect("the by-name tail");
        assert!(arm < template && template < tail);
        assert!(body[..arm].contains("!callee_has_compiled_code && !compiled_entry_forbidden"));
        let f = body_of(&src, "\nunsafe fn mic_tail_call_selected_declaring_body(");
        for needle in [
            "select_for_jit_receiver(",
            "compiled.requires_wrapped_entry || compiled.has_indy_trap",
            "build_bytecode_callee_cached(",
            "template.declaring_class_id != decl_id",
            "\"CRATONVM_JIT_MIC_TAIL_DECLARING_BODY\"",
            "call_body_the_mic_probe_just_found(",
            "publish_mic_rust_cached_entry(",
            // nio21 w20 L2: a table-free callee also reaches the machine-code IC.
            "if !has_table && !rerun_pins",
            "mic_tail_publish_declaring_body(",
        ] {
            assert!(f.contains(needle), "{needle}");
        }
        let p = body_of(&src, "\nunsafe fn mic_tail_publish_declaring_body(");
        for needle in [
            "\"CRATONVM_JIT_MIC_TAIL_DECLARING_BODY_IC\"",
            "mic_publication_skipped(pic_ptr)",
            "publish_mega_dispatch_entry(",
            "withdraw_ic_publication_if_redefined(",
        ] {
            assert!(p.contains(needle), "{needle}");
        }
    }

    /// JIT r20 w19 yLadder (DISP20-7): the MIC miss arm's probe takes the
    /// declaring-class body as its answer before it records a refusal, and the
    /// fallback keeps the entryless arm's rules behind its own switch.
    #[test]
    fn the_mic_miss_probe_falls_back_to_the_declaring_class_body() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let body = body_of(&src, "\nunsafe fn jit_invoke_virtual_mic_body(");
        let miss = &body[body
            .find("// --- Cache miss: full resolution + update cache ---")
            .expect("the miss arm")..];
        let fallback = miss
            .find("probe_selected_declaring_body(vm, info, receiver_class_id)")
            .expect("the miss arm asks the fallback");
        let declined = miss
            .find("if probed.is_none()")
            .expect("the refusal record");
        let published = miss
            .find("mark_site_rerun_pins_before_publish(")
            .expect("publication");
        assert!(fallback < declined && declined < published);
        let f = body_of(&src, "\nfn probe_selected_declaring_body(");
        for needle in [
            "select_for_jit_receiver(",
            "decl_id == receiver_class_id",
            "method.is_synchronized()",
            "compiled.requires_wrapped_entry || compiled.has_indy_trap",
            "build_bytecode_callee_cached(",
            "template.declaring_class_id != decl_id",
            "\"CRATONVM_JIT_PROBE_DECLARING_BODY\"",
        ] {
            assert!(f.contains(needle), "{needle}");
        }
    }
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
/// `--jdk-only` with the real-field `ThreadLocal` natives), with the class ids
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
    // `--jdk-only` only, the mode gate of the full layout's
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
            if cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JIT_IR_INHERITABLE_THREAD_LOCAL",
            ) && l.itl.as_u32() != 0
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
/// ON**, `=0` restores the `--jdk-only`-only answer: the referent queries of
/// [`jit_thread_local_ir_layout`] answer a `--compatible` VM too when it runs
/// real class bytes (not `use_synthetic_jdk`) and its loaded
/// `java/lang/ref/Reference` is no compatibility stub and declares `referent`
/// as its first instance field ([`reference_class_with_referent_first`]).
/// Then the optimizing tier's `refersTo` expansion runs in both modes (host
/// t1 wave 11: `--compatible` soft 1264.9 / phantom 1182.6 ns per 16 calls
/// on the call path vs HotSpot 4.4). Sound for the reason the `--jdk-only`
/// answer is: every `refersTo` body either mode can run -- the JDK's
/// `refersTo` -> `refersToImpl` -> `refersTo0`, or the `JdkReferences`
/// family's `native_ref_refers_to` stand-in when that family is reverted --
/// is the identity compare of slot `REF_FIELD_REFERENT` without a keep-alive.
fn refers_to_compatible_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_REFERS_TO_COMPATIBLE")
}

/// t1 wave 12 (lane REFS4): may the referent queries of
/// [`jit_thread_local_ir_layout`] answer `vm`? `--jdk-only` always (the
/// t1 wave 9 rule); `--compatible` under [`refers_to_compatible_enabled`].
fn refers_to_referent_query_serves(vm: &SharedVm) -> bool {
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
fn reference_class_with_referent_first(vm: &SharedVm) -> Option<ClassId> {
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
///   keep-alive) -- wherever the referent read gate can never arm
///   (`VmHeap::zgc_referent_gate_may_arm`: Gen, G1, and a ZGC heap without
///   the after-release referent clear). There the checked helper's
///   no-keep-alive read answers the slot word itself, as the inline read does.
fn reference_referent_row_words(
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
    let compare_only = if heap.zgc_referent_gate_may_arm() { 0 } else { row };
    let class = reference.map_or(0, |c| u64::from(c.as_u32()));
    [0, handed_out, class, compare_only]
}

/// t1 wave 12 (lane REFS4, `CRATONVM_JIT_IR_REFERS_TO_COMPACT_ROW` /
/// `CRATONVM_JIT_IR_REFERS_TO_COMPATIBLE`): the referent-row query answers
/// where the index query does, names no row without a loaded `Reference`,
/// and a `--compatible` VM without a loaded real `Reference` declines both
/// queries without a write.
#[cfg(test)]
mod t1w12_refs4_referent_row_query_tests {
    use super::*;

    fn ask_row(
        vm: Option<&SharedVm>,
    ) -> (i64, [u64; cratonvm_jit::direct_helpers::REFERENCE_REFERENT_ROW_QUERY_WORDS]) {
        let mut words = [7u64; cratonvm_jit::direct_helpers::REFERENCE_REFERENT_ROW_QUERY_WORDS];
        let vm_ptr = vm.map_or(0, |vm| vm as *const SharedVm as i64);
        let got = unsafe {
            jit_thread_local_ir_layout(
                vm_ptr,
                words.as_mut_ptr() as i64,
                cratonvm_jit::direct_helpers::REFERENCE_REFERENT_ROW_QUERY_WORDS as i64,
            )
        };
        (got, words)
    }

    #[test]
    fn t1w12_refs4_jdk_only_answers_three_words() {
        let mut config = crate::config::VmConfig::default();
        config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
        config.use_synthetic_jdk = false;
        let vm = SharedVm::new(config);
        let (got, words) = ask_row(Some(&vm));
        // j27 wave 5 (lane REFS): four words now (the compare-only row).
        assert_eq!(got, 4);
        assert_eq!(words[0], 0, "REF_FIELD_REFERENT");
        // A fresh VM has no `Reference` loaded: no class, so no row.
        if words[2] == 0 {
            assert_eq!((words[1], words[3]), (0, 0));
        }
        // j27 wave 5 (lane REFS): off ZGC the two rows are one answer.
        if vm.mem.heap.is_generational() || vm.mem.heap.is_g1() {
            assert_eq!(words[3], words[1]);
        }
    }

    /// j27 wave 5 (lane REFS): the referent-gate query answers the heap's
    /// own fact in one word, in both modes; no VM declines without a write.
    #[test]
    fn j27_refs_the_gate_query_answers_the_heap() {
        let ask = |vm: Option<&SharedVm>| {
            let n = cratonvm_jit::direct_helpers::REFERENT_GATE_MAY_ARM_QUERY_WORDS;
            let mut words = vec![7u64; n];
            let vm_ptr = vm.map_or(0, |vm| vm as *const SharedVm as i64);
            let got = unsafe {
                jit_thread_local_ir_layout(vm_ptr, words.as_mut_ptr() as i64, n as i64)
            };
            (got, words[0])
        };
        for mode in [
            cratonvm_types::compat::CompatibilityMode::JdkOnly,
            cratonvm_types::compat::CompatibilityMode::Compatible,
        ] {
            let mut config = crate::config::VmConfig::default();
            config.compatibility_mode = mode;
            // `--jdk-only` refuses the synthetic library (as the jdk-only test above).
            config.use_synthetic_jdk = mode != cratonvm_types::compat::CompatibilityMode::JdkOnly
                && config.use_synthetic_jdk;
            let vm = SharedVm::new(config);
            let want = u64::from(vm.mem.heap.zgc_referent_gate_may_arm());
            assert_eq!(ask(Some(&vm)), (1, want));
        }
        assert_eq!(ask(None), (0, 7));
    }

    /// j27 wave 5 (lane REFS): the answer's rows, by collector -- the
    /// compare-only row is named exactly where the referent gate never arms,
    /// the handed-out row on Gen / G1 alone; the index and the class id are
    /// the same answer either way.
    #[test]
    fn j27_refs_the_compare_only_row_follows_the_referent_gate() {
        let mut config = crate::config::VmConfig::default();
        config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
        config.use_synthetic_jdk = false;
        let vm = SharedVm::new(config);
        let words = reference_referent_row_words(&vm);
        let (_, asked) = ask_row(Some(&vm));
        assert_eq!(&asked[..], &words[..], "the query writes these words");
        let heap = &vm.mem.heap;
        if heap.zgc_referent_gate_may_arm() {
            assert_eq!(words[3], 0, "a gate that may arm: no compare-only row");
        } else {
            let gen_or_g1 = heap.is_generational() || heap.is_g1();
            assert_eq!(words[1], if gen_or_g1 { words[3] } else { 0 });
        }
    }

    #[test]
    fn t1w12_refs4_compatible_without_reference_and_no_vm_decline() {
        let vm = SharedVm::new(crate::config::VmConfig::default());
        assert_eq!(ask_row(Some(&vm)), (0, [7, 7, 7, 7]));
        assert_eq!(ask_row(None), (0, [7, 7, 7, 7]));
        // The compatible arm, switched off, declines as before too.
        let got = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_REFERS_TO_COMPATIBLE", Some("0"))],
            || ask_row(Some(&vm)),
        );
        assert_eq!(got, (0, [7, 7, 7, 7]));
    }

    #[test]
    fn t1w12_refs4_the_row_query_size_is_its_own() {
        let n = cratonvm_jit::direct_helpers::REFERENCE_REFERENT_ROW_QUERY_WORDS;
        assert_ne!(n, cratonvm_jit::direct_helpers::REFERENCE_REFERENT_QUERY_WORDS);
        assert_ne!(n, cratonvm_jit::direct_helpers::THREAD_LOCAL_RECEIVER_QUERY_WORDS);
        assert_ne!(n, cratonvm_jit::direct_helpers::THREAD_LOCAL_LAYOUT_PENDING_QUERY_WORDS);
        assert!(n < cratonvm_jit::direct_helpers::THREAD_LOCAL_IR_LAYOUT_WORDS);
    }
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
fn reference_get_ir_layout_words(
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

#[cfg(test)]
mod nio21w19_thread_local_ir_layout_tests {
    use super::*;

    /// The producer's signature is the alias the compiler transmutes to, and
    /// a null VM, a null buffer or a short buffer answer `0` without a write.
    #[test]
    fn nio21w19_the_layout_fn_refuses_without_a_vm_or_a_buffer() {
        let _: cratonvm_jit::direct_helpers::DirectHelperFnThreadLocalIrLayout =
            jit_thread_local_ir_layout;
        let mut buf = [7u64; cratonvm_jit::direct_helpers::THREAD_LOCAL_IR_LAYOUT_WORDS];
        let out = buf.as_mut_ptr() as i64;
        let n = buf.len() as i64;
        unsafe {
            assert_eq!(jit_thread_local_ir_layout(0, out, n), 0);
            assert_eq!(jit_thread_local_ir_layout(0x1000, 0, n), 0);
            assert_eq!(jit_thread_local_ir_layout(0x1000, out, n - 1), 0);
        }
        assert!(buf.iter().all(|&w| w == 7), "nothing written");
    }

    /// The table carries the producer (process-lived), and a `--compatible`
    /// VM, where the thin helper never serves, answers `0`.
    #[test]
    fn nio21w19_the_table_publishes_it_and_compatible_answers_zero() {
        assert_eq!(
            direct_helper_table().thread_local_ir_layout,
            jit_thread_local_ir_layout as *const () as usize
        );
        let vm = SharedVm::new(crate::config::VmConfig::default());
        let mut buf = [0u64; cratonvm_jit::direct_helpers::THREAD_LOCAL_IR_LAYOUT_WORDS];
        let got = unsafe {
            jit_thread_local_ir_layout(
                &vm as *const SharedVm as i64,
                buf.as_mut_ptr() as i64,
                buf.len() as i64,
            )
        };
        assert_eq!(got, 0);
    }
}

/// Interpreter round i3 wave 81, lane L3 (`CRATONVM_JIT_REFLECT_DOOR_DIRECT`,
/// `CRATONVM_REFLECT_DOOR_PHASE_STATS`). Text pins: the helper cannot be
/// driven without compiled code.
#[cfg(test)]
mod i81_l3_reflect_door_direct_tests {
    fn body_of(src: &str, sig: &str) -> String {
        let start = src.find(sig).expect("the function exists");
        let rest = &src[start + 1..];
        rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)].to_string()
    }

    /// The direct reflective arm is asked behind its switch, after the native
    /// site cache (which keeps every site it serves) and before the MIC
    /// resolution and the by-name cascade it exists to skip.
    #[test]
    fn the_direct_reflect_arm_sits_between_the_site_cache_and_the_mic() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let body = body_of(&src, "\nunsafe fn jit_invoke_virtual_mic_body(");
        let site = body
            .find("try_jit_site_cached_native_dispatch(")
            .expect("site cache");
        let gate = body.find("jit_reflect_door_direct()").expect("the switch");
        let arm = body
            .find("jit_forced_reflect_door_native(")
            .expect("the arm");
        let mic = body
            .find("let mic = &*(mic_ptr as *const JitMICSlot);")
            .expect("the MIC");
        let tail = body
            .find("invoke_or_native_keeping_args_current(")
            .expect("the cascade");
        assert!(site < gate && gate < arm && arm < mic && mic < tail);
    }

    /// The census stamps the helper's entry before any door and its return
    /// after the panic scope, both behind the census switch.
    #[test]
    fn the_census_stamps_both_ends_of_the_virtual_mic_helper() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let body = body_of(&src, "\npub unsafe extern \"C\" fn jit_invoke_virtual_mic(");
        let gate = body
            .find(".reflect_door_phase_stats()")
            .expect("the census switch");
        let entry = body
            .find("ReflectDoorGatePoint::HelperEntry")
            .expect("entry stamp");
        let door = body.find("try_ffm_direct_door(").expect("the first door");
        // The panic scope after the first door: nio21's profile-seed note
        // (before the census) opens a `contain(` of its own.
        let scope = body[door..]
            .find("contain(")
            .map(|i| i + door)
            .expect("the panic scope");
        let exit = body
            .rfind("ReflectDoorGatePoint::HelperExit")
            .expect("exit stamp");
        assert!(gate < entry && entry < door && door < scope && scope < exit);
        // Wave 83: the lean door's early return stamps its exit too.
        let lean = body.find("try_lean_native_door(").expect("the lean door");
        let lean_exit = body
            .find("ReflectDoorGatePoint::HelperExit")
            .expect("lean exit stamp");
        assert!(lean < lean_exit && lean_exit < scope);
    }

    /// Interpreter round i3 wave 83, lane L3: the direct arm files its site in
    /// the lean table behind its own switch, before it serves the call, and
    /// the record is a plain callback of no capability (the arm's own call has
    /// none).
    #[test]
    fn the_direct_arm_files_a_lean_record_behind_its_switch() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let body = body_of(&src, "\nunsafe fn jit_invoke_virtual_mic_body(");
        let arm = body
            .find("jit_forced_reflect_door_native(")
            .expect("the arm");
        let gate = body.find("jit_reflect_door_lean_site").expect("the switch");
        let fill = body.find("reflect_door_lean_site_fill(").expect("the fill");
        let call = body[arm..]
            .find("safe_native_call(")
            .map(|i| i + arm)
            .expect("the call");
        assert!(arm < gate && gate < fill && fill < call);
        let fill_fn = body_of(&src, "\nfn reflect_door_lean_site_fill(");
        assert!(fill_fn.contains("capability_kind: None"));
        assert!(fill_fn.contains("kind: LeafNativeKind::Callback"));
        assert!(fill_fn.contains("lean_native_site_diagnostics_on()"));
    }
}

#[cfg(test)]
mod infallible_direct_native_tests {
    use super::*;
    #[test]
    fn an_oom_after_a_native_side_effect_is_never_replayed() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(crate::ThreadId(0), "direct-native-oom");
        let mut ctx = crate::vm::NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        let calls = std::cell::Cell::new(0);
        let pins = ctx.thread.native_pin_roots.len();
        let answer = jit_native_alloc_once(&mut ctx, |ctx| {
            calls.set(calls.get() + 1); // The native side effect has happened.
            let object = ctx
                .shared
                .mem
                .heap
                .try_alloc_object_full(ClassId::new(0), 0)
                .unwrap();
            ctx.thread.native_pin_roots.push(object);
            crate::runtime::native_oom::raise(40, "object")
        });
        assert_eq!(calls.get(), 1);
        assert_eq!(ctx.thread.native_pin_roots.len(), pins);
        assert!(matches!(
            answer,
            Some(Err(crate::error::MethodCallFailed::InternalError(
                crate::error::VmError::Runtime(crate::error::RuntimeError::OutOfMemoryError { .. })
            )))
        ));
        assert!(!crate::runtime::native_oom::unwind_ok());
    }
}

#[cfg(test)]
mod r21_reflect_accessor_link_tests {
    //! JIT round 21 wave 1, lane reflect (`CRATONVM_JIT_REFLECT_ACCESSOR_LINK`).
    use super::{
        reflect_accessor_site_params, reflect_accessor_split, reflect_accessor_wrapper,
        JitInvokeInfo, MH_LINK_HIDDEN_TAG, REFLECT_CTOR_SITES, REFLECT_WRAPPER_ORDER,
    };

    #[test]
    fn the_wrapper_id_slots_follow_the_wrapper_table() {
        // `ReflectLinkIds::wrappers` is indexed by this order; every entry
        // names a wrapper, and the eight are distinct.
        let mut names: Vec<&str> = REFLECT_WRAPPER_ORDER
            .iter()
            .map(|b| reflect_accessor_wrapper(*b).expect("a wrapper"))
            .collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 8);
    }

    #[test]
    fn constructor_sites_are_the_specialized_arms() {
        for (p, site) in REFLECT_CTOR_SITES.iter().enumerate() {
            assert_eq!(site.matches("Ljava/lang/Object;").count(), p + 1);
        }
        assert!(!REFLECT_CTOR_SITES.contains(&"([Ljava/lang/Object;)Ljava/lang/Object;"));
    }

    #[test]
    fn a_hidden_leaf_tag_never_reads_as_an_interned_info() {
        // Interned infos are at least 8-aligned, so a tagged word is never
        // mistaken for one by the by-name route (`mh_link_leaf`).
        assert!(std::mem::align_of::<JitInvokeInfo>() >= 8);
        assert_ne!(MH_LINK_HIDDEN_TAG & 7, 0);
        assert_ne!(MH_LINK_HIDDEN_TAG, 1);
    }

    #[test]
    fn only_the_specialized_accessor_sites_are_linked() {
        let o = "Ljava/lang/Object;";
        for p in 0..4usize {
            let site = format!("({})Ljava/lang/Object;", o.repeat(p + 1));
            assert_eq!(reflect_accessor_site_params(&site), Some(p));
        }
        // The spreading arm, the caller-sensitive arms and anything typed.
        assert_eq!(
            reflect_accessor_site_params("(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;"),
            None
        );
        assert_eq!(
            reflect_accessor_site_params("(Ljava/lang/Object;Ljava/lang/Class;)Ljava/lang/Object;"),
            None
        );
        assert_eq!(reflect_accessor_site_params("(II)I"), None);
        assert_eq!(
            reflect_accessor_site_params(&format!("({})Ljava/lang/Object;", o.repeat(5))),
            None
        );
    }

    #[test]
    fn member_descriptors_split_into_parameters_and_return() {
        let mut out = [""; 3];
        assert_eq!(reflect_accessor_split("(II)I", &mut out), Some((2, b'I')));
        assert_eq!(&out[..2], &["I", "I"]);
        let mut out = [""; 3];
        assert_eq!(
            reflect_accessor_split("(Ljava/lang/String;[[IJ)V", &mut out),
            Some((3, b'V'))
        );
        assert_eq!(out, ["Ljava/lang/String;", "[[I", "J"]);
        let mut out = [""; 3];
        assert_eq!(reflect_accessor_split("()[Lp/X;", &mut out), Some((0, b'[')));
        // Wider than the buffer, or malformed.
        let mut out = [""; 3];
        assert_eq!(reflect_accessor_split("(IIII)V", &mut out), None);
        assert_eq!(reflect_accessor_split("(Ljava/lang/String)V", &mut out), None);
        assert_eq!(reflect_accessor_split("II)V", &mut out), None);
        assert_eq!(reflect_accessor_split("(I)", &mut out), None);
    }

    #[test]
    fn primitive_parameters_take_exactly_their_wrapper() {
        assert_eq!(reflect_accessor_wrapper(b'I'), Some("java/lang/Integer"));
        assert_eq!(reflect_accessor_wrapper(b'C'), Some("java/lang/Character"));
        assert_eq!(reflect_accessor_wrapper(b'D'), Some("java/lang/Double"));
        assert_eq!(reflect_accessor_wrapper(b'L'), None);
        assert_eq!(reflect_accessor_wrapper(b'['), None);
    }
}

/// JIT round 21 wave 1 (lane indy, INDY21-2, `CRATONVM_JIT_IR_CLINIT_BARRIER`):
/// the class-init barrier's well-known answers.
#[cfg(test)]
mod r21_indy_clinit_barrier_answer_tests {
    use super::*;

    /// The thread-id offset is `JvmThread::thread_id`'s plus one; class id 0
    /// and an initialized class answer `0`; any other code is not ours.
    #[test]
    fn the_clinit_barrier_words_are_answered_only_for_their_codes() {
        use cratonvm_jit::ir_lower as jl;
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let vm: &SharedVm = &shared;
        assert_eq!(
            clinit_barrier_well_known_answer(vm, jl::WELL_KNOWN_JVM_THREAD_ID_OFFSET),
            Some(std::mem::offset_of!(JvmThread, thread_id) as i64 + 1)
        );
        assert_eq!(
            clinit_barrier_well_known_answer(
                vm,
                jl::vh_const_query(jl::WELL_KNOWN_CLINIT_OWNER_CELL_TAG, 0, 0)
            ),
            Some(0),
            "class id 0"
        );
        assert_eq!(
            clinit_barrier_well_known_answer(
                vm,
                jl::vh_const_query(jl::WELL_KNOWN_CLINIT_BASE_CELL_TAG, 0, 3)
            ),
            Some(0)
        );
        assert_eq!(
            clinit_barrier_well_known_answer(
                vm,
                jl::vh_const_query(jl::WELL_KNOWN_VH_CONST_PLAN_TAG, 7, 0)
            ),
            None,
            "a VarHandle query is not ours"
        );
        assert_eq!(clinit_barrier_well_known_answer(vm, 1), None);
    }
}

/// JIT round 21 wave 3, lane intrinsics (INTRINSICS21-6,
/// `CRATONVM_JIT_VECTORIZED_HASH_UNROLL`): the four-per-step fold answers
/// exactly what the one-per-step fold answers, for every element kind the JDK
/// passes, every length around the unroll boundary, and every seed.
#[cfg(test)]
mod r21_intrinsics_vectorized_hash_unroll_tests {
    use super::*;

    #[test]
    fn r21_unrolled_fold_equals_the_scalar_fold() {
        let mut bytes = vec![0u8; 4 * 41];
        let mut x: u32 = 0x9E37_79B9;
        for b in bytes.iter_mut() {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *b = (x >> 24) as u8;
        }
        for (step, basic_type) in [
            (1usize, VH_T_BOOLEAN),
            (1, VH_T_BYTE),
            (2, VH_T_CHAR),
            (2, VH_T_SHORT),
            (4, VH_T_INT),
        ] {
            for elems in 0..=41usize {
                for seed in [0i32, 1, -1, i32::MIN, 0x1234_5678] {
                    let slice = &bytes[..elems * step];
                    assert_eq!(
                        vectorized_hash_fold_unrolled(slice, step, basic_type, seed),
                        vectorized_hash_fold(slice, step, basic_type, seed),
                        "step {step} type {basic_type} elems {elems} seed {seed}"
                    );
                }
            }
        }
        // The refusal is the scalar fold's too.
        assert_eq!(vectorized_hash_fold_unrolled(&[1, 2], 2, VH_T_INT, 1), None);
    }
}

/// JIT round 21 wave 4 (lane compat, COMPAT21-1): the thin-helper gate asks the
/// resolver the generic tail asks, so a `Bridge` standing in front of concrete
/// loaded bytecode is refused by the helper exactly when the tail yields it.
#[cfg(test)]
mod r21_compat_thin_helper_asks_the_resolver {
    use super::*;
    use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState};
    use cratonvm_native_api::NativeKind;
    use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
    use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    use cratonvm_reader::method::ClassFileMethod;

    const OWNER: &str = "cratonvm/r21compat/Owner";
    const WITH_CODE: &str = "r21CompatBody";
    const NO_DECL: &str = "r21CompatUndeclared";
    const DESC: &str = "()I";

    fn never_called(
        _ctx: &mut dyn cratonvm_native_api::NativeContext,
        _args: &[cratonvm_types::Value],
    ) -> Result<Option<cratonvm_types::Value>, cratonvm_types::error::MethodCallFailed> {
        Ok(None)
    }

    /// `name()I` with the body `iconst_1; ireturn`.
    fn method_with_code(name: &str) -> ClassFileMethod {
        ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: std::sync::Arc::from(name),
            descriptor: std::sync::Arc::from(DESC),
            attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 1,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x04, 0xac]),
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }
    }

    /// Define `OWNER` (application loader) declaring `WITH_CODE()I` only.
    fn define_owner(shared: &SharedVm) -> ClassId {
        let loader = ClassLoaderId::Application;
        let mut cm = shared.classes.class_manager.write();
        let id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id,
            loader_id: loader,
            name: cratonvm_types::intern_arc(OWNER),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Initialized,
            initializing_thread: None,
            constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![method_with_code(WITH_CODE)],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: cratonvm_classloading::ClassOrigin::default(),
            has_finalizer: false,
            code_source: None,
            array_info: None,
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        });
        cm.register_class_name(loader, OWNER, id);
        id
    }

    fn vm_with(kind: NativeKind) -> SharedVm {
        let mut shared = SharedVm::new(crate::config::VmConfig::default());
        for name in [WITH_CODE, NO_DECL] {
            shared
                .natives
                .native_methods
                .register_with_kind(OWNER, name, DESC, never_called, kind);
        }
        shared
    }

    /// A `Bridge` in front of loaded concrete bytecode: the tail
    /// (`invoke_or_native`, via `registered_native_yields_to_loaded_bytecode`)
    /// yields it, so the thin-helper gate must refuse it. The same `Bridge`
    /// with no declaration behind it is what the tail runs, so it is admitted,
    /// and so is a class that is not loaded at all.
    #[test]
    fn a_bridge_over_loaded_bytecode_is_refused_like_the_tail_yields_it() {
        let shared = vm_with(NativeKind::Bridge);
        // Not loaded yet: the tail admits (no declaration to prefer).
        assert!(!thin_helper_resolver_yields(
            &shared,
            OWNER,
            WITH_CODE,
            DESC,
            NativeKind::Bridge
        ));
        define_owner(&shared);
        assert!(
            thin_helper_resolver_yields(&shared, OWNER, WITH_CODE, DESC, NativeKind::Bridge),
            "resolve_dispatch step 3: concrete bytecode beats a Bridge in both modes"
        );
        assert!(
            crate::vm::registered_native_yields_to_loaded_bytecode(
                &shared,
                OWNER,
                WITH_CODE,
                DESC,
                never_called,
                NativeKind::Bridge,
            ),
            "the tail's own question gives the same answer"
        );
        assert!(
            !thin_helper_resolver_yields(&shared, OWNER, NO_DECL, DESC, NativeKind::Bridge),
            "no declaration: nothing to yield to, the tail runs the Bridge"
        );
        if thin_helper_asks_resolver_enabled(&shared) {
            assert!(
                direct_helper_refusal(&shared, OWNER, WITH_CODE, DESC, None, Some(NativeKind::Bridge)),
                "the gate refuses what the tail yields"
            );
        }
        assert!(
            !direct_helper_refusal(&shared, OWNER, NO_DECL, DESC, None, Some(NativeKind::Bridge)),
            "and still admits the Bridge the tail runs"
        );
    }

    /// A reviewed `Intrinsic` may shadow concrete bytecode (step 2), so the
    /// gate keeps admitting it over loaded bytecode, and the resolver is not
    /// even asked outside the opt-in pure-bytecode probe.
    #[test]
    fn a_reviewed_intrinsic_over_loaded_bytecode_is_still_admitted() {
        let shared = vm_with(NativeKind::Intrinsic);
        define_owner(&shared);
        assert!(!thin_helper_resolver_yields(
            &shared,
            OWNER,
            WITH_CODE,
            DESC,
            NativeKind::Intrinsic
        ));
        assert!(!direct_helper_refusal(
            &shared,
            OWNER,
            WITH_CODE,
            DESC,
            None,
            Some(NativeKind::Intrinsic)
        ));
    }

    /// COMPAT21-2: an unclassified gate memoises under any installed policy;
    /// a classified one only with none installed (its answer is the funnel's
    /// recording capability check). Before the fix the rule was "no policy
    /// installed" for every gate, and `vm_init` always installs one.
    #[test]
    fn the_gate_memo_engages_for_unclassified_gates_under_an_installed_policy() {
        assert!(gate_memo_policy_admits(false, true), "the production case");
        assert!(gate_memo_policy_admits(false, false));
        assert!(gate_memo_policy_admits(true, false));
        assert!(
            !gate_memo_policy_admits(true, true),
            "a classified gate's per-call capability check records, never memoised"
        );
    }

    /// JIT r23 wave 8 (lane W8-L2, CF-W8-1): the split path for a classified
    /// gate asks the capability half on every call, BEFORE any memo read, and
    /// memoises only the policy half (`direct_helper_refusal` with no
    /// capability question). A source pin, because the order is the whole
    /// correctness argument and a VM-free unit test cannot reach the cell.
    #[test]
    fn the_classified_gate_split_asks_the_capability_half_before_the_memo() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let start = src
            .find("\nfn classified_gate_refuses_split(")
            .expect("classified_gate_refuses_split must exist");
        let body = &src[start..start + src[start..].find("\n}\n").expect("closed item")];
        let cap = body
            .find("thin_helper_capability_admits(")
            .expect("the per-call capability question");
        let memo = body.find("GATE_DECISION_MEMO").expect("the memo read");
        assert!(cap < memo, "the capability half must run before the memo read");
        assert!(
            body.contains("direct_helper_refusal(vm, class_name, method_name, descriptor, None, kind)"),
            "only the policy half may be memoised"
        );
        assert!(body.contains("thin_helper_answer_settled("));
    }

    /// COMPAT21-2: the memo key moves with the registry generation (a new
    /// triple), so a memoised answer never outlives the registration it read.
    #[test]
    fn the_gate_memo_key_moves_with_the_registry_generation() {
        let mut shared = vm_with(NativeKind::Bridge);
        let before = gate_memo_key(&shared);
        assert_eq!(before.1, shared.jit.jit_cache.redefine_epoch());
        shared.natives.native_methods.register_with_kind(
            OWNER,
            "r21CompatAnother",
            DESC,
            never_called,
            NativeKind::Bridge,
        );
        assert_ne!(gate_memo_key(&shared).0, before.0);
    }

    /// COMPAT21-1/2: an answer the resolver term may still change is not
    /// memoised. A `Bridge` on a class that is not loaded is unsettled; once
    /// the declaration with its decoded `Code` is loaded it is settled. An
    /// `Intrinsic` (outside the pure probe) and an unregistered triple never
    /// reach the term, so they are settled at once with no class lookup.
    #[test]
    fn only_a_settled_resolver_answer_is_memoised() {
        let shared = vm_with(NativeKind::Bridge);
        if thin_helper_asks_resolver_enabled(&shared) {
            assert!(!thin_helper_answer_settled(
                &shared,
                OWNER,
                WITH_CODE,
                DESC,
                Some(NativeKind::Bridge)
            ));
        }
        define_owner(&shared);
        assert!(thin_helper_answer_settled(
            &shared,
            OWNER,
            WITH_CODE,
            DESC,
            Some(NativeKind::Bridge)
        ));
        assert!(
            thin_helper_answer_settled(&shared, OWNER, NO_DECL, DESC, Some(NativeKind::Bridge)),
            "a loaded class that declares nothing for the triple is final"
        );
        assert!(thin_helper_answer_settled(
            &shared,
            "cratonvm/r21compat/NotLoaded",
            WITH_CODE,
            DESC,
            Some(NativeKind::Intrinsic)
        ));
        assert!(thin_helper_answer_settled(
            &shared,
            "cratonvm/r21compat/NotLoaded",
            WITH_CODE,
            DESC,
            None
        ));
    }

    /// COMPAT21-3: the compile gates' question, without its opt-in switch. A
    /// `Bridge` in front of loaded concrete bytecode yields (the interpreter
    /// and the tail run the bytecode, so it is no shadow); the same `Bridge`
    /// with no declaration behind it runs; an `Intrinsic` is never yielded.
    #[test]
    fn will_run_asks_the_resolver_for_non_intrinsic_registrations() {
        use crate::runtime::interpreter::registration_yields_to_loaded_bytecode as yields;
        let bridge = vm_with(NativeKind::Bridge);
        define_owner(&bridge);
        assert!(yields(&bridge, OWNER, WITH_CODE, DESC, never_called, NativeKind::Bridge));
        assert!(!yields(&bridge, OWNER, NO_DECL, DESC, never_called, NativeKind::Bridge));
        let intrinsic = vm_with(NativeKind::Intrinsic);
        define_owner(&intrinsic);
        assert!(!yields(
            &intrinsic,
            OWNER,
            WITH_CODE,
            DESC,
            never_called,
            NativeKind::Intrinsic
        ));
        // The switch is default ON since r23 (R23-AB-FLIP); `=0` restores the
        // allow-list answer (a `Bridge` "runs").
        if !crate::runtime::interpreter::will_run_asks_resolver_enabled(&bridge) {
            assert!(crate::runtime::interpreter::registered_native_will_run(
                &bridge, OWNER, WITH_CODE, DESC
            ));
        } else {
            assert!(!crate::runtime::interpreter::registered_native_will_run(
                &bridge, OWNER, WITH_CODE, DESC
            ));
        }
    }

    /// COMPAT21-4: the force interceptor runs a forced triple's native only
    /// for an `Intrinsic`, and the pure-bytecode probe yields even that under
    /// `--jdk-only` -- `resolve_native_dispatch_wave1(.., true, true)` arm for
    /// arm. The seal arm asks the same.
    #[test]
    fn the_forced_seal_asks_what_the_interceptor_would_run() {
        use crate::runtime::interpreter::{forced_kind_would_run, forced_native_would_run};
        use cratonvm_native_api::unretire::dial_kind_decision;
        for jdk_only in [false, true] {
            for pure in [false, true] {
                for kind in [NativeKind::Bridge, NativeKind::SyntheticStub, NativeKind::Intrinsic] {
                    let expected = match kind {
                        NativeKind::Intrinsic => !(jdk_only && pure),
                        _ => false,
                    };
                    assert_eq!(
                        forced_kind_would_run(jdk_only, kind, pure),
                        expected,
                        "forced_kind_would_run mismatch for jdk_only={jdk_only}, kind={:?}, pure={pure}",
                        kind
                    );
                    if kind == NativeKind::Intrinsic {
                        assert_eq!(!dial_kind_decision(kind, pure) || !jdk_only, expected);
                    }
                }
            }
        }

        let bridge = vm_with(NativeKind::Bridge);
        assert!(!forced_native_would_run(&bridge, OWNER, WITH_CODE, DESC));
        let stub = vm_with(NativeKind::SyntheticStub);
        assert!(!forced_native_would_run(&stub, OWNER, WITH_CODE, DESC));
        let intrinsic = vm_with(NativeKind::Intrinsic);
        assert!(forced_native_would_run(&intrinsic, OWNER, WITH_CODE, DESC));
        assert!(!forced_native_would_run(
            &intrinsic,
            OWNER,
            "r21CompatUnregistered",
            DESC
        ));
    }

    /// rev-vmjit item 3: the three r21 compat switches are `SharedVm` fields
    /// read once at construction, not process `OnceLock`s, so a VM built under
    /// a per-thread override keeps its own answer.
    #[test]
    fn the_r21_compat_switches_are_read_per_vm() {
        let vm_with = |value: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_JIT_THIN_HELPER_ASKS_RESOLVER", value),
                    ("CRATONVM_JIT_WILL_RUN_ASKS_RESOLVER", value),
                    ("CRATONVM_JIT_FORCED_SEAL_ASKS_RESOLVER", value),
                ],
                || SharedVm::new(crate::config::VmConfig::default()),
            )
        };
        let unset = vm_with(None);
        let off = vm_with(Some("0"));
        let on = vm_with(Some("1"));
        // Default ON, `0` kills it.
        assert!(thin_helper_asks_resolver_enabled(&unset));
        assert!(!thin_helper_asks_resolver_enabled(&off));
        assert!(thin_helper_asks_resolver_enabled(&on));
        use crate::runtime::interpreter::{
            forced_seal_asks_resolver_enabled, will_run_asks_resolver_enabled,
        };
        // R23-AB-FLIP: default ON, `0` kills it.
        assert!(will_run_asks_resolver_enabled(&unset));
        assert!(!will_run_asks_resolver_enabled(&off));
        assert!(will_run_asks_resolver_enabled(&on));
        // Opt-in: only `1` is on.
        assert!(!forced_seal_asks_resolver_enabled(&unset));
        assert!(!forced_seal_asks_resolver_enabled(&off));
        assert!(forced_seal_asks_resolver_enabled(&on));
    }
}

/// JIT r22 wave 3 (lane inline): text pins for the constant bind's
/// `checkCustomized` count (the helper cannot be driven without compiled code).
#[cfg(test)]
mod r22_inline_mh_bind_customization_tests {
    fn body_of(src: &str, sig: &str) -> String {
        let start = src.find(sig).expect("the function exists");
        let rest = &src[start + 1..];
        rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)].to_string()
    }

    /// Both routes the redirect serves (a `bindTo` handle and the plain one)
    /// ask the count, for a real handle only, right before they return.
    #[test]
    fn both_served_routes_count_a_real_handle_first() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let body = body_of(&src, "\nunsafe fn mh_constant_bind_redirect(");
        let ask = "&& !mh_bind_customization_serves(vm, info_ptr as usize, args[0])";
        assert_eq!(body.matches(ask).count(), 2, "both routes count");
        let bound_ask = body.find(ask).expect("the bound route's count");
        let bound_ret = body.find("bound: Some(receiver as i64),").expect("the bound route");
        assert!(bound_ask < bound_ret);
        let plain_ask = body.rfind(ask).expect("the plain route's count");
        let plain_ret = body.rfind("bound: None,").expect("the plain route");
        assert!(plain_ask < plain_ret);
        let helper = body_of(&src, "\nunsafe fn mh_bind_customization_serves(");
        assert!(helper.contains("lang_invoke::lane_check_customized("));
        assert!(helper.contains(".is_some()"));
        // JIT r22 retirement: the customized steady state is answered first,
        // from the raw slots, before the context is built.
        let fast = helper.find("mh_bind_handle_is_customized(").expect("the raw steady-state test");
        let ctx = helper.find("NativeContextImpl {").expect("the counting path");
        assert!(fast < ctx);
    }

    /// Item 4 of `r22-review-w3-intrinsics-on-inline-FIXED-20261020.md`: the bound
    /// site's publication re-checks the redefinition epoch AFTER it installs.
    #[test]
    fn the_bound_site_publication_rechecks_the_epoch_after_installing() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let body = body_of(&src, "\nunsafe fn publish_bound_site_entry(");
        let update = body.find("mic.update(").expect("the MIC install");
        let install = body.find("pic.install(").expect("the PIC install");
        let recheck = body
            .find("withdraw_ic_publication_if_redefined(vm, epoch_now, mic, pic)")
            .expect("the re-check");
        assert!(update < recheck && install < recheck);
    }
}

/// JIT round 23 wave 1, lane L6 (SYNC22-3): the `--compatible` bind-time
/// refusal of a thin helper whose triple the registry holds no row for.
#[cfg(test)]
mod r23_w1_l6_compat_bind_asks_registry_tests {
    use super::*;
    use cratonvm_native_api::NativeKind;

    const OWNER: &str = "cratonvm/r23compat/Owner";
    const DESC: &str = "()I";

    fn never_called(
        _ctx: &mut dyn cratonvm_native_api::NativeContext,
        _args: &[cratonvm_types::Value],
    ) -> Result<Option<cratonvm_types::Value>, cratonvm_types::error::MethodCallFailed> {
        Ok(None)
    }

    fn vm_with(value: Option<&str>) -> SharedVm {
        // Fast boot off: it skips `SyntheticStub` rows of `cratonvm/` classes,
        // and the stub row below is one of the kinds under test.
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_COMPAT_BIND_ASKS_REGISTRY", value),
                ("CRATONVM_COMPAT_FAST_BOOT_JIT", Some("0")),
            ],
            || SharedVm::new(crate::config::VmConfig::default()),
        )
    }

    /// Default ON, `0` kills it, read once per VM.
    #[test]
    fn the_switch_is_read_per_vm() {
        assert!(compat_bind_asks_registry_enabled(&vm_with(None)));
        assert!(!compat_bind_asks_registry_enabled(&vm_with(Some("0"))));
        assert!(compat_bind_asks_registry_enabled(&vm_with(Some("1"))));
    }

    /// `--compatible` (the embedding default): an unregistered triple is
    /// refused, a registered one of any kind is admitted as before.
    #[test]
    fn compat_refuses_only_an_unregistered_triple() {
        let mut vm = vm_with(None);
        // `VmConfig::default()` is `--compatible` (the embedding default).
        // A loaded site class with no superclass row: nothing the tail could
        // reach (the review fix-back asks the superclass walk).
        {
            let mut cm = vm.classes.class_manager.write();
            let owner = cm
                .try_ensure_synthetic_class(OWNER, 0)
                .expect("Compatible mode fabricates this fixture");
            cm.set_superclass(owner, None);
        }
        assert!(compat_bind_refuses_unregistered(&vm, OWNER, "r23Unregistered", DESC));
        assert!(!direct_native_bind_admitted(&vm, OWNER, "r23Unregistered", DESC));
        for (name, kind) in [
            ("r23Stub", NativeKind::SyntheticStub),
            ("r23Bridge", NativeKind::Bridge),
            ("r23Intrinsic", NativeKind::Intrinsic),
        ] {
            vm.natives
                .native_methods
                .register_with_kind(OWNER, name, DESC, never_called, kind);
            assert!(!compat_bind_refuses_unregistered(&vm, OWNER, name, DESC), "{name}");
            assert!(direct_native_bind_admitted(&vm, OWNER, name, DESC), "{name}");
        }
    }

    /// Review fix-back (L12 page `r23-w1-review-compat-bind-asks-exact-site-triple`):
    /// a site class with no row of its own whose SUPERCLASS registers the native
    /// is admitted (the tail's superclass walk runs that native); a chain with no
    /// row is refused; a class the VM has not loaded is not refused.
    #[test]
    fn an_inherited_row_is_admitted_from_the_site_class() {
        const PARENT: &str = "cratonvm/r23compat/Parent";
        const CHILD: &str = "cratonvm/r23compat/Child";
        let mut vm = vm_with(None);
        {
            let mut cm = vm.classes.class_manager.write();
            let parent = cm
                .try_ensure_synthetic_class(PARENT, 0)
                .expect("Compatible mode fabricates this fixture");
            let child = cm
                .try_ensure_synthetic_class(CHILD, 0)
                .expect("Compatible mode fabricates this fixture");
            cm.set_superclass(parent, None);
            cm.set_superclass(child, Some(parent));
        }
        vm.natives
            .native_methods
            .register_with_kind(PARENT, "r23Inherited", DESC, never_called, NativeKind::Bridge);
        assert!(!compat_bind_refuses_unregistered(&vm, CHILD, "r23Inherited", DESC));
        assert!(direct_native_bind_admitted(&vm, CHILD, "r23Inherited", DESC));
        assert!(compat_bind_refuses_unregistered(&vm, CHILD, "r23Nowhere", DESC));
        assert!(!direct_native_bind_admitted(&vm, CHILD, "r23Nowhere", DESC));
        assert!(!compat_bind_refuses_unregistered(
            &vm,
            "cratonvm/r23compat/NeverLoaded",
            "r23Nowhere",
            DESC
        ));
    }

    /// Off: the old `--compatible` answer, every unclassified triple admitted.
    #[test]
    fn off_restores_the_unconditional_compat_admission() {
        let vm = vm_with(Some("0"));
        assert!(!compat_bind_refuses_unregistered(&vm, OWNER, "r23Unregistered", DESC));
        assert!(direct_native_bind_admitted(&vm, OWNER, "r23Unregistered", DESC));
    }
}

/// JIT round 21 lane stress (FUZZ21-1 / FUZZ21-7): how the safepoint slow
/// path classifies an arrival for the stress switches.
#[cfg(test)]
mod r21_stress_poll_arrival_tests {
    use super::*;

    fn vm() -> SharedVm {
        SharedVm::new(crate::config::VmConfig::default())
    }

    /// FUZZTOOL21-1: the exit line `jit-stress-battery.sh` parses, its
    /// `refused` total the sum of the reasons, one census per VM.
    #[test]
    fn the_census_line_sums_refusals_and_is_per_vm() {
        let shared = vm();
        let other = vm();
        let c = &shared.jit.bridge.poll_stress_census;
        for _ in 0..5 {
            PollStressCensus::bump(&c.arrivals);
        }
        PollStressCensus::bump(&c.granted);
        PollStressCensus::bump(&c.refused_unnamed);
        PollStressCensus::bump(&c.refused_not_polling_body);
        PollStressCensus::bump(&c.refused_not_polling_body);
        PollStressCensus::bump(&c.refused_chain);
        PollStressCensus::bump(&c.collections);
        let line = c.census_line();
        assert!(
            line.starts_with("[cratonvm-deopt] FUZZ21-1 forced arrivals=5 granted=1 refused=4 "),
            "{line}"
        );
        assert!(line.contains(" refused-not-polling-body=2 "), "{line}");
        assert!(line.contains(" refused-chain=1 "), "{line}");
        assert!(line.contains(" collections=1 "), "{line}");
        assert!(
            other
                .jit
                .bridge
                .poll_stress_census
                .census_line()
                .starts_with("[cratonvm-deopt] FUZZ21-1 forced arrivals=0 granted=0 refused=0 "),
            "another VM's census is its own"
        );
    }

    #[test]
    fn a_natural_arrival_or_a_foreign_flag_never_reads_a_switch() {
        let shared = vm();
        let barrier = &shared.mem.gc_barrier;
        let addr = barrier.stw_requested_flag_addr() as usize as i64;
        let other = std::hint::black_box(0u8);
        let foreign = &other as *const u8 as usize as i64;
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_DBG_JIT_DEOPT_A_LOT", Some("1")),
                ("CRATONVM_DBG_GC_AT_BACK_EDGE", Some("1")),
            ],
            || {
                assert_eq!(
                    poll_stress_arrival(&shared, 0),
                    PollStressArrival::default()
                );
                assert_eq!(
                    poll_stress_arrival(&shared, foreign),
                    PollStressArrival::default(),
                    "a flag address that is not this VM's is never read"
                );
                // The flag byte directly (`CacheLineFlag::store` is the
                // barrier's own): this private VM registers no poll words, so
                // no pause count can desync.
                barrier
                    .stw_requested
                    .flag()
                    .store(true, std::sync::atomic::Ordering::Release);
                let natural = poll_stress_arrival(&shared, addr);
                barrier
                    .stw_requested
                    .flag()
                    .store(false, std::sync::atomic::Ordering::Release);
                assert_eq!(natural, PollStressArrival::default(), "a pause is natural");
                assert_eq!(
                    poll_stress_arrival(&shared, addr),
                    poll_stress_arrival_from(true, true),
                    "the positive control: clear flag, both switches"
                );
            },
        );
    }

    #[test]
    fn a_forced_arrival_acts_on_each_switch_that_is_set() {
        let shared = vm();
        let addr = shared.mem.gc_barrier.stw_requested_flag_addr() as usize as i64;
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_DBG_JIT_DEOPT_A_LOT", None),
                ("CRATONVM_DBG_GC_AT_BACK_EDGE", None),
            ],
            || {
                assert_eq!(
                    poll_stress_arrival(&shared, addr),
                    PollStressArrival::default()
                )
            },
        );
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_DBG_JIT_DEOPT_A_LOT", Some("100:7")),
                ("CRATONVM_DBG_GC_AT_BACK_EDGE", Some("0")),
            ],
            || {
                assert_eq!(
                    poll_stress_arrival(&shared, addr),
                    poll_stress_arrival_from(true, false)
                )
            },
        );
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_DBG_JIT_DEOPT_A_LOT", Some("off")),
                ("CRATONVM_DBG_GC_AT_BACK_EDGE", Some("on")),
            ],
            || {
                assert_eq!(
                    poll_stress_arrival(&shared, addr),
                    poll_stress_arrival_from(false, true)
                )
            },
        );
    }
}
