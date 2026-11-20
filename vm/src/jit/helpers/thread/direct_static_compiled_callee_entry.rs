#[allow(unused_imports)]
use super::*;


// These two gate the RUNTIME dispatch helper's OWN direct-entry cache
// (`jit_invoke_dispatch`'s `DISPATCH_CACHE`/`jit_cache` lookups and
// `jit_invoke_virtual_mic`'s cached `entry_ptr` fast path) — NOT the same
// thing as `cratonvm_jit::direct_jit_callee_calls_enabled` (which gates
// compile-time eager-callee-compile in `jit/src/lib.rs` and the inline
// MIC/PIC codegen in `jit/src/x64.rs`, and is default-ON: see that
// function's doc comment for the closed RBP-mirror race and the
// general-throughput regression its default avoids).
//
// `statically_bound` (this function's only caller-side gate, besides this
// flag) is `matches!(info.invoke_kind, 1 | 3)` — invokespecial/invokestatic,
// whose target is fully determined by the constant-pool entry under JVM
// semantics, with no receiver-class ambiguity. Default-ON: measured on
// `bench/BenchSuite.java` bintrees16 (self-recursive `static Node make(int)`/
// `static long check(Node)`, the classic allocation+recursion micro-
// benchmark) at 43.7s with this flag off vs. 2.8s on — every invokestatic/
// invokespecial call site was paying the full `jit_invoke_dispatch` helper
// round trip instead of a cached direct entry. invokestatic/invokespecial
// are among the most common call forms in ordinary Java (constructors,
// private/static helpers, `super` calls), so leaving this off by default is
// a severe, general JIT throughput regression, not a narrow one.
//
// Formerly known residual, now FIXED: the static CP-owner cache was not a
// sound target resolver for every invokespecial/static BRIDGE specifically
// (Lucene DataOutput's `writeByte` bridge resolved as `Object.writeByte` was
// the originally-observed case) — a synthetic-bridge target-resolution gap,
// not a receiver-ambiguity one. Root cause: `info.class_name` (and
// `try_jit_compile_callee_slow`'s `class_name` parameter more generally) is
// the literal constant-pool-referenced class for an `invokespecial` site,
// but that is NOT always the class JVMS §6.5 says method *selection* should
// start searching from — for a genuine `super.m(...)` call (ACC_SUPER set on
// the calling class, target not `<init>`, CP-referenced class a genuine
// superclass of the caller), selection restarts at the CALLING class's own
// direct superclass instead. A class between the caller and the far-off
// CP-referenced ancestor that overrides the method (a compiler-generated
// bridge, or an ordinary override) was walked straight past, landing on a
// much-less-specific declaration higher up the chain. Fixed by computing the
// JVMS-correct selection-start class at JIT-compile time (before either
// `DISPATCH_CACHE` or `jit_cache` ever see the site) via
// `classloading::invokespecial_selection_start`, applied identically by both
// the interpreter (`interpreter::invokespecial_owner_class_name`) and the
// JIT compiler (`jit::try_compile_with_invokespecial_resolver`'s
// `cp_invokespecial_owner_resolver`), so the two execution modes agree and
// neither the per-callsite `DISPATCH_CACHE` nor the global `jit_cache` can
// ever cache a target resolved from the wrong starting class.
#[inline]
pub fn direct_static_compiled_callee_entry_enabled() -> bool {
    pub static CACHE: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&CACHE, || {
        match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}

// The virtual-call counterpart of the flag above. Default-ON now that every
// compilation path owns MIC/PIC slots, cache publication rejects class-only
// profile seeds, both entry ABIs are lowered, and the generated caller
// republishes its active frame after a raw call. Opt out for diagnosis with
// `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY=0`.
//
// What the default-OFF period cost, measured independently on 2026-07-27 (H2
// `org/h2/` ban residuals): this flag gates the ONLY write of
// `mic.cached_entry_word`, so with it off the inline MIC/PIC cascade the codegen
// emits can never open and every virtual call out of compiled code falls back
// through `invoke_or_native` into the INTERPRETER. Compiling a method therefore
// made its callees slower, and compiling more of a program made it slower
// overall. On H2 `TestFreeSpace` -- `org/h2/mvstore/FreeSpaceBitSet.toString`
// compiled, its `java/util/BitSet.nextClearBit` callee compiled too -- the
// `toString` scan loop cost 5582 ms per 2000 calls with this off and 169 ms
// with it on, and the off-cost grew with the callee's working set while the
// on-cost did not.
//
// Consequence for anything verified while it was off: a JIT ban whose mechanism
// is compiled-to-compiled virtual dispatch could not reproduce during a
// default-OFF run, because the dispatch it guards was inert. JASPER-JDT.2/.3
// (`org/eclipse/jdt/internal/compiler/parser/` and `ast/`) were removed on
// 2026-07-26 on exactly such runs and had to be restored on 2026-07-27, when
// turning this on brought the miscompile straight back. They were only removed
// for good on 2026-07-28, after the defect behind them was root-caused (the
// LICM / speculative pre-header bypass fixed in `613b10f4c`) and re-measured
// with this ON -- see their entry in `skip_list.rs`. Re-verify any similar
// removal with this ON.
#[inline]
pub fn direct_virtual_compiled_callee_entry_enabled() -> bool {
    pub static CACHE: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&CACHE, || {
        match Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
        {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}

// ---------------------------------------------------------------------------
// Thread-local JvmThread pointer for JIT helper access
// ---------------------------------------------------------------------------

thread_local! {
    /// DIAGNOSTIC: name of the most recently dispatched JIT callee on this
    /// thread (`class.method desc`). Set at the top of `jit_invoke_dispatch` /
    /// `jit_invoke_virtual_mic`. Read by `jit_putfield_int` when it sees a
    /// non-canonical receiver, to name the miscompiled method. Only touched on
    /// dispatch (cheap) and never in release-critical inner loops.
    pub static CURRENT_JIT_CALLEE: std::cell::RefCell<String> =
        const { std::cell::RefCell::new(String::new()) };

    /// Stores a raw pointer to the current thread's JvmThread.
    /// Safety invariant: only ONE `&mut JvmThread` is derived from this at a time,
    /// and only within a single JIT helper call scope. The pointer is set before
    /// entering JIT code and cleared immediately after.
    pub static JIT_THREAD: Cell<*mut JvmThread> = const { Cell::new(std::ptr::null_mut()) };

    /// ALL out-of-band JIT→interpreter signals for this thread, consolidated
    /// in ONE thread-local. `execute_jit_call`'s post-return drain used to
    /// pay SIX separate thread-local accesses (pending exception, AIOOBE,
    /// arithmetic, NPE, NPE action, deopt flag) on EVERY JIT invocation —
    /// ~50-60ns, the single largest constant in the interpreter→JIT entry
    /// overhead on short-callee shapes. One struct = one TLS address
    /// computation for the whole drain (`take_all_jit_signals`).
    ///
    /// **This block holds NO heap references.** The one signal that did —
    /// `exception`, the pending Java throwable — now lives on
    /// [`JvmThread::jit_pending_exception`], because a `thread_local!` is
    /// unreachable from a collecting thread and the throwable was therefore
    /// neither scanned nor remapped for the whole stash→drain window (see
    /// `jit-signals-root-gap.md`). Every remaining field is a
    /// plain scalar the collector has no interest in, which is why they may
    /// stay here and keep the one-TLS-access drain. **Do not add an
    /// `ObjectRef`, a `Value`, or a raw heap address to this struct** — put it
    /// on `JvmThread` and root it in `memory/roots.rs` + `memory/gc.rs`.
    ///
    /// Field semantics (formerly the individual statics):
    /// * `athrow_bci` — see the field; the bci of the `athrow` that produced
    ///   the (thread-resident) pending exception.
    /// * `aioobe` — pending AIOOBE `(index, length)` from a JIT bounds
    ///   check (`jit_throw_aioobe`), consumed on the `i64::MIN` sentinel.
    /// * `arithmetic` — pending `ArithmeticException` ("/ by zero") from
    ///   the integer-division zero-divisor guard; drained like `aioobe`
    ///   (throwing through the exception table instead of re-running the
    ///   method, which double-executed prior side effects).
    /// * `npe` — pending NullPointerException from a JIT array helper on a
    ///   null array reference; drained like `aioobe`.
    /// * `npe_action` — JEP 358 (partial): the *operation kind* that raised
    ///   the pending JIT NPE (`helpful_npe::jit_action` code, `0` = none),
    ///   set in lockstep with `npe` so the drain can attach an action-only
    ///   message; reset by every bare `set_jit_pending_npe()` so a stale
    ///   code never leaks onto an unrelated NPE.
    /// * `deopt` — the out-of-band deopt/exception signal (`i64::MIN`
    ///   sentinel collision disambiguation): every JIT path producing the
    ///   `i64::MIN` deopt sentinel ALSO sets this, so a method legitimately
    ///   returning `Long.MIN_VALUE` is not mistaken for a deopt (which
    ///   would spuriously re-run it, double-executing side effects).
    pub static JIT_SIGNALS: JitSignals = const {
        JitSignals {
            athrow_bci: Cell::new(-1),
            athrow_owner: Cell::new(0),
            aioobe: Cell::new(None),
            arithmetic: Cell::new(false),
            npe: Cell::new(false),
            npe_action: Cell::new(0),
            trap_frames: std::cell::RefCell::new(None),
            deopt: Cell::new(false),
            frameless_trap: Cell::new(FramelessTrapSite::Unknown),
            redispatch_escape: Cell::new(0),
            trap_snapshot_deferred: Cell::new(DeferredTrapSnapshot::None),
            trap_snapshot_bound: Cell::new(usize::MAX),
        }
    };

    /// Debug-only reentrancy guard for [`jit_thread_mut`]. Set while a
    /// `&mut JvmThread` handed out by `jit_thread_mut` is considered live, and
    /// cleared when the [`JitThreadGuard`] returned alongside it is dropped.
    /// A nested/aliasing `jit_thread_mut` call observes the set flag and trips
    /// the `debug_assert!`. Compiled out entirely in release builds, so release
    /// behaviour is unchanged.
    #[cfg(debug_assertions)]
    pub static JIT_THREAD_BORROWED: Cell<bool> = const { Cell::new(false) };

    /// Debug-only: where the currently-live `jit_thread_mut` borrow was taken.
    ///
    /// The `debug_assert!` in `jit_thread_mut` says two borrows overlap; it
    /// cannot say WHICH two, and with 58 call sites in this file that is the
    /// whole difficulty. Populated only when
    /// `CRATONVM_DBG_JIT_BORROW_SITES=1`, because capturing a backtrace on
    /// every borrow costs far more than the borrow.
    ///
    /// `Cell<Option<Box<..>>>` rather than `RefCell`: the one place this is
    /// read is the aliasing trip itself, which is by definition a reentrant
    /// moment, and a `RefCell` double-borrow panic there would replace the
    /// diagnosis with a different panic.
    #[cfg(debug_assertions)]
    pub static JIT_THREAD_BORROW_SITE: Cell<Option<Box<std::backtrace::Backtrace>>> =
        const { Cell::new(None) };
}

/// Debug-only: is the borrow-site backtrace capture switched on?
#[cfg(debug_assertions)]
pub fn jit_borrow_site_capture_enabled() -> bool {
    pub static CACHE: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&CACHE, || false)}

/// Debug-only: snapshot the borrow flag and clear it, so a nested JIT entry
/// (the interpreter re-entering JIT from inside a bail) starts a fresh borrow
/// level. Returns the previous value for [`restore_jit_borrow`]. No-op in
/// release builds.
///
/// Carries the recorded borrow SITE along with the flag. Without that, the
/// nested case leaves `flag = true` restored over `site = None` (the inner
/// guard's drop cleared it), and a trip in the outer level then reports "no
/// site recorded" while the capture is switched on — an instrument that goes
/// quiet exactly where it is needed.
#[cfg(debug_assertions)]
pub fn suspend_jit_borrow() -> (bool, Option<Box<std::backtrace::Backtrace>>) {
    let prev = JIT_THREAD_BORROWED.with(|b| {
        let prev = b.get();
        b.set(false);
        prev
    });
    let site = JIT_THREAD_BORROW_SITE.with(|s| s.take());
    (prev, site)
}

/// Debug-only: restore the borrow flag suspended by [`suspend_jit_borrow`]
/// once the nested JIT call has returned. No-op in release builds.
#[cfg(debug_assertions)]
pub fn restore_jit_borrow(prev: (bool, Option<Box<std::backtrace::Backtrace>>)) {
    let (flag, site) = prev;
    JIT_THREAD_BORROWED.with(|b| b.set(flag));
    JIT_THREAD_BORROW_SITE.with(|s| s.set(site));
}

/// Debug-only RAII guard that marks the `jit_thread_mut` borrow as released
/// when dropped. In release builds this is a zero-sized no-op.
pub struct JitThreadGuard {
    #[cfg(debug_assertions)]
    pub _private: (),
}

impl Drop for JitThreadGuard {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        JIT_THREAD_BORROWED.with(|b| b.set(false));
        #[cfg(debug_assertions)]
        JIT_THREAD_BORROW_SITE.with(|s| s.set(None));
    }
}

/// Opaque token returned by [`set_jit_thread`] and consumed by
/// [`restore_jit_thread`]. Bundles the previously-stored raw thread pointer
/// with the suspended debug borrow flag, so a strictly-nested JIT re-entry
/// (outer `jit_invoke_dispatch` → interpreter bail → inner
/// `jit_invoke_dispatch`) gets a clean borrow level while the outer borrow is
/// frozen on the call stack for the nested call's duration.
pub struct JitThreadScope {
    pub prev_ptr: *mut JvmThread,
    /// Shadow-stack `top` watermark captured at JIT entry (follow-up §3). On exit
    /// [`restore_jit_thread`] resets `top` to this value, healing any push an
    /// abnormal JIT exit (exception/deopt that skipped a method epilogue) left
    /// unbalanced. `None` when the shadow-stack gate is off (no-op). On a normal
    /// exit the per-method epilogues already restored `top`, so the reset is a
    /// no-op there.
    pub saved_shadow_top: Option<usize>,
    #[cfg(debug_assertions)]
    pub prev_borrow: (bool, Option<Box<std::backtrace::Backtrace>>),
}

/// DIAGNOSTIC: read the current dispatched JIT callee name.
pub fn current_jit_callee() -> String {
    CURRENT_JIT_CALLEE.with(|c| c.borrow().clone())
}

/// Return a replacement JIT argument buffer when a safepoint has forwarded an
/// object argument.  The JIT ABI carries raw addresses, so canonicalizing only
/// the receiver is insufficient for signatures such as `([BII)V`: a moved
/// byte array can otherwise be decoded from its recycled pre-GC address.
///
/// The common case allocates nothing.  `num_jit_args` counts compact native
/// values (including an instance receiver), so each parsed descriptor parameter
/// advances one buffer position even for category-2 Java values.
pub fn forward_jit_reference_args(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args: &[i64],
) -> Option<Vec<i64>> {
    let mut replacement: Option<Vec<i64>> = None;
    for_each_jit_reference_arg(info, args, |index, raw| {
        forward_jit_arg_at(vm, args, &mut replacement, index, raw);
    });
    replacement
}

/// [`forward_jit_reference_args`] that also answers WHICH argument words it
/// validated as live heap bases: bit `i` of the mask is set when buffer index
/// `i` (`i < 64`) of the buffer the caller goes on to use -- the replacement
/// when there is one, else `args` -- names an object `load_and_forward_checked`
/// accepted (the same structural predicate as `is_object_address`).
///
/// JIT round 19 wave 2, lane funnel2 (FUNNEL17-2,
/// `CRATONVM_JIT_NATIVE_WALK_ONCE`): the native-site door then builds those
/// references from the word instead of walking the heap a second time. The
/// mask is only good until the next GC point; both dispatch helpers run none
/// between this pass and the door.
pub fn forward_jit_reference_args_validated(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args: &[i64],
) -> (Option<Vec<i64>>, u64) {
    let mut replacement: Option<Vec<i64>> = None;
    let mut mask = 0u64;
    for_each_jit_reference_arg(info, args, |index, raw| {
        if raw == 0 || (raw as u64 & 0x7) != 0 || (raw as u64) >= (1u64 << 48) {
            return;
        }
        // SAFETY: as `forward_jit_arg_at`: a descriptor-declared reference
        // word that passed the canonicality checks above.
        let object = unsafe { ObjectRef::from_raw(raw as usize as *mut u8) };
        let (forwarded, valid) = vm.mem.heap.load_and_forward_checked(object);
        let forwarded = forwarded.as_ptr() as i64;
        if forwarded != raw {
            let buf = replacement.get_or_insert_with(|| args.to_vec());
            buf[index] = forwarded;
        }
        if valid && index < 64 {
            mask |= 1u64 << index;
        }
    });
    (replacement, mask)
}

/// A reference word the forwarding pass already validated
/// ([`forward_jit_reference_args_validated`]'s mask), as an `ObjectRef`
/// without a second heap walk; `None` when the bit is clear.
#[inline]
pub fn prevalidated_jit_ref(prevalidated: u64, index: usize, raw: i64) -> Option<ObjectRef> {
    if index < 64 && prevalidated & (1u64 << index) != 0 && raw != 0 {
        // SAFETY: the forwarding pass validated this exact word as a live
        // object base, and no GC point ran since (the mask's contract).
        Some(unsafe { ObjectRef::from_raw(raw as usize as *mut u8) })
    } else {
        None
    }
}


/// Visit every reference position of a compact JIT argument buffer: the
/// receiver (when [`jit_info_has_receiver`] says there is one) and each `L`/`[` parameter of
/// `info.descriptor`. `visit(index, raw)` gets the buffer index and its word.
///
/// The one descriptor walk shared by [`forward_jit_reference_args`] and
/// [`CompileArgPins::pin_raw`], so the two can never disagree about which
/// words are references.
pub fn for_each_jit_reference_arg(
    info: &JitInvokeInfo,
    args: &[i64],
    mut visit: impl FnMut(usize, i64),
) {
    // JIT round 23 wave 3d (lane L1): the receiver rule is
    // `jit_info_has_receiver`, the same one the argument decoder asks. This
    // read `invoke_kind != 3`, which named argument 0 of a STATIC direct
    // self-call row (kind 4) a receiver and shifted every descriptor
    // parameter one buffer word to the right.
    let has_receiver = jit_info_has_receiver(info);
    let mut arg_index = usize::from(has_receiver);

    // Instance receivers are object references regardless of their descriptor.
    if has_receiver {
        if let Some(&raw) = args.first() {
            visit(0, raw);
        }
    }

    let bytes = info.descriptor.as_bytes();
    let mut p = match bytes.iter().position(|&b| b == b'(') {
        Some(i) => i + 1,
        None => return,
    };
    while p < bytes.len() && bytes[p] != b')' && arg_index < args.len() {
        let is_ref = matches!(bytes[p], b'L' | b'[');
        if is_ref {
            visit(arg_index, args[arg_index]);
        }
        match bytes[p] {
            b'L' => {
                while p < bytes.len() && bytes[p] != b';' {
                    p += 1;
                }
                p = p.saturating_add(1);
            }
            b'[' => {
                while p < bytes.len() && bytes[p] == b'[' {
                    p += 1;
                }
                if p < bytes.len() && bytes[p] == b'L' {
                    while p < bytes.len() && bytes[p] != b';' {
                        p += 1;
                    }
                    p = p.saturating_add(1);
                } else {
                    p = p.saturating_add(1);
                }
            }
            _ => p += 1,
        }
        arg_index += 1;
    }
}

/// A JIT argument word that names a live heap object, validated. Null,
/// misaligned and non-canonical words (and anything the heap does not
/// recognise as an object base) answer `None`.
#[inline]
pub fn validated_jit_ref(vm: &SharedVm, raw: i64) -> Option<ObjectRef> {
    let bits = raw as u64;
    if bits == 0 || (bits & 0x7) != 0 || bits >= (1u64 << 48) {
        return None;
    }
    vm.mem.heap.is_object_address(bits as usize)
}

/// Reference arguments rooted in the current thread's `native_pin_roots`
/// across a synchronous callee compile (r11-irlower-outgoing-arg-staging-refs-unpublished).
///
/// The dispatch helpers read their arguments out of the compiled caller's
/// outgoing staging block. That block is in no oop map, and neither are the
/// helper's own copies (`forward_jit_reference_args`'s `Vec`, a decoded
/// `Value` vector). A callee compile can resolve fields and load classes,
/// i.e. run `<clinit>` and collect, so a moving collection inside it leaves
/// every one of those copies naming the pre-move address — which the helper
/// then hands to the freshly compiled callee. Re-forwarding afterwards is not a
/// repair: `load_and_forward` needs a forwarding word at the old address, and
/// ZGC's slide leaves none. Pinning is: `native_pin_roots` is scanned and
/// remapped with the thread, so reading the pins back yields the current
/// addresses.
///
/// Push and release are watermark-based (`base`), the same discipline every
/// other `native_pin_roots` user follows, so nested pins inside the compile
/// compose.
///
/// # Released on every exit (gen r5w4/jit8)
///
/// The explicit releases below read the pins back and truncate. Until wave 4
/// they were the ONLY release: a scope left any other way -- a `compile()` or
/// `call()` that unwound into the helper's `contain`, or the "cannot happen"
/// arm of [`compile_with_ref_args_pinned`] / [`call_compiled_then_route`]
/// whose `jit_thread_mut()` came back `None` -- left the arguments rooted
/// "until the next watermark release below them". On the outermost thread
/// scope there is no such release: every later pin scope takes its watermark
/// ABOVE the orphans, so they stayed roots for the life of the thread, and a
/// dropped structure an argument reached stayed reachable
/// (`docs/feature-designs/deopt-osr.md`,
/// candidate 1). `Drop` now truncates to the watermark whenever an explicit
/// release has not run.
///
/// Liveness argument (why the implicit release drops nothing the program can
/// still reach): the pins exist only so the helper can READ its arguments back
/// after a GC point inside the scope. When the scope ends without an explicit
/// release, the helper has stopped reading them -- it is unwinding, or it took
/// the arm that already gave up reading them back -- and every value in
/// `native_pin_roots[base..]` was pushed by this scope or by a scope nested in
/// it, all of which have ended. A reference the program still holds is held
/// by its own frame (the compiled caller's map / shadow push, the interpreter
/// frame, the callee's parameter home), exactly as on the normal path, where
/// the explicit release truncates the same range.
///
/// The truncation goes through the thread pointer captured at pin time (the
/// thread whose `native_pin_roots` was pushed), not through
/// [`jit_thread_mut`]: `Drop` can run while the helper's own `&mut JvmThread`
/// borrow is still on the stack, and a second `jit_thread_mut` there would
/// trip its aliasing assertion in a debug build. The write happens only on
/// the exits that skipped the explicit release, never beside a live use of
/// that borrow.
pub struct CompileArgPins {
    /// `native_pin_roots.len()` before the first pin: the release watermark.
    pub base: usize,
    /// The argument index of each pin, in push order.
    pub indices: smallvec::SmallVec<[usize; INLINE_JIT_NATIVE_ARGS]>,
    /// The thread the pins were pushed onto, until an explicit release has
    /// truncated them; null afterwards. Read only by `Drop`.
    pub owner: *mut JvmThread,
}

impl CompileArgPins {
    /// Pin every validated reference word of a compact JIT argument buffer.
    pub fn pin_raw(vm: &SharedVm, thread: &mut JvmThread, info: &JitInvokeInfo, args: &[i64]) -> Self {
        let base = thread.native_pin_roots.len();
        let mut indices: smallvec::SmallVec<[usize; INLINE_JIT_NATIVE_ARGS]> =
            smallvec::SmallVec::new();
        for_each_jit_reference_arg(info, args, |index, raw| {
            if let Some(obj) = validated_jit_ref(vm, raw) {
                thread.native_pin_roots.push(obj);
                indices.push(index);
            }
        });
        Self {
            base,
            indices,
            owner: thread as *mut JvmThread,
        }
    }

    /// Pin every validated object in a decoded argument vector.
    pub fn pin_values(vm: &SharedVm, thread: &mut JvmThread, values: &[Value]) -> Self {
        let base = thread.native_pin_roots.len();
        let mut indices: smallvec::SmallVec<[usize; INLINE_JIT_NATIVE_ARGS]> =
            smallvec::SmallVec::new();
        for (index, value) in values.iter().enumerate() {
            if let Value::Object(Some(obj)) = value {
                if let Some(obj) = vm.mem.heap.is_object_address(obj.as_ptr() as usize) {
                    thread.native_pin_roots.push(obj);
                    indices.push(index);
                }
            }
        }
        Self {
            base,
            indices,
            owner: thread as *mut JvmThread,
        }
    }

    /// Truncate `thread`'s pins to the watermark and disarm `Drop`: the one
    /// place an explicit release ends the scope.
    pub fn truncate_and_disarm(&mut self, thread: &mut JvmThread) {
        thread.native_pin_roots.truncate(self.base);
        self.owner = std::ptr::null_mut();
    }

    /// The argument buffer as it stands NOW, pins kept: `Some(copy)` with the
    /// moved words replaced when a collection moved any pinned object since
    /// [`Self::pin_raw`], `None` when every word is still current.
    pub fn current_raw(&self, thread: &JvmThread, args: &[i64]) -> Option<Vec<i64>> {
        let mut moved: Option<Vec<i64>> = None;
        for (k, &index) in self.indices.iter().enumerate() {
            let (Some(pinned), Some(&old)) =
                (thread.native_pin_roots.get(self.base + k), args.get(index))
            else {
                continue;
            };
            let now = pinned.as_ptr() as usize as i64;
            if now != old {
                if let Some(slot) = moved.get_or_insert_with(|| args.to_vec()).get_mut(index) {
                    *slot = now;
                }
            }
        }
        moved
    }

    /// Release the pins, answering the argument buffer as it stands NOW
    /// (see [`Self::current_raw`]).
    pub fn release_raw(mut self, thread: &mut JvmThread, args: &[i64]) -> Option<Vec<i64>> {
        let moved = self.current_raw(thread, args);
        self.truncate_and_disarm(thread);
        moved
    }

    /// Release the pins without reading them back.
    pub fn release(mut self, thread: &mut JvmThread) {
        self.truncate_and_disarm(thread);
    }

    /// Release the pins, writing each pinned object's current address back
    /// into `values`.
    pub fn release_values(mut self, thread: &mut JvmThread, values: &mut [Value]) {
        for (k, &index) in self.indices.iter().enumerate() {
            if let (Some(&pinned), Some(slot)) = (
                thread.native_pin_roots.get(self.base + k),
                values.get_mut(index),
            ) {
                *slot = Value::Object(Some(pinned));
            }
        }
        self.truncate_and_disarm(thread);
    }
}

impl Drop for CompileArgPins {
    /// The implicit release (gen r5w4/jit8): see "Released on every exit" on
    /// the type. A no-op after any explicit release.
    fn drop(&mut self) {
        if self.owner.is_null() {
            return;
        }
        // SAFETY: `owner` is the `JvmThread` this scope pushed onto, captured
        // from a live `&mut` at pin time. A `CompileArgPins` never outlives
        // the helper call that created it (every one is a local of a JIT
        // helper or of a function it calls, never stored), and the thread
        // outlives every helper call made on it. Reaching here means no
        // explicit release ran, i.e. the scope is being left by an unwind or
        // by an arm that stopped reading the pins; the helper's own borrow of
        // the thread is not in use at this instant (it is the frame being
        // dropped, or a caller frame suspended in the call that is dropping
        // this one), so this is the only access.
        let thread = unsafe { &mut *self.owner };
        if thread.native_pin_roots.len() > self.base {
            thread.native_pin_roots.truncate(self.base);
        }
        self.owner = std::ptr::null_mut();
    }
}

// gcg g6/jm (2026-10-02) -- `gcd-d9d-call-argument-copies-still-rooted-on-other-call-shapes`,
// row 3. The whole census of `Gcd1ArgPinSpecialProbe more settle` (vg3, g3a)
// names `native_pin_roots` -- a dispatch door's `CompileArgPins` -- as the only
// holder of a 16 MiB argument the callee had already dropped, with the caller
// compiled. The doors pin a call's arguments so that a sentinel's service can
// RE-RUN the callee with current words (its handler, or from entry after a
// declined stash). The hit and PIC arms pin only when the site once published
// a callee with an exception table (`JitMICSlot::callee_rerun_pins`, sticky),
// and the megamorphic table arm pinned ALWAYS. A body that owns its arguments
// (`cratonvm_jit::callee_body_owns_its_arguments`: no handler, no frameless
// trap stub, no indy trap, not synchronized) cannot take the handler arm and
// has no frameless trap; what is left is a re-run after a DECLINED stash -- the
// same exposure every hit / PIC arm already accepts for a handler-free callee,
// where it pins nothing. So for such a body these doors now pin nothing either,
// and the argument dies with the callee's own use of it, as in HotSpot.

/// `CRATONVM_JIT_DOOR_OWNED_ARGS_UNPINNED` -- opt-in (gcg g6/jm; vg6 jm6_argsp
/// on = off, so not the probe's door); off keeps the unconditional
/// megamorphic-arm pin and the site-flag pins of the hit / PIC arms. Read ONCE per VM, when its `JitCache` is built
/// (`JitCache::door_owned_args_unpinned`): this is asked on every door call.
#[inline]
pub fn door_owned_args_unpinned_enabled(vm: &SharedVm) -> bool {
    vm.jit.jit_cache.door_owned_args_unpinned()
}

/// Must a Rust dispatch door pin this call's arguments across the callee
/// (see the section above)? `wanted` is the door's own reason (the site flag,
/// or `true` for the megamorphic arm); `callee_owns` says whether the body it
/// is about to enter owns its arguments; `unpinned_on` is
/// [`door_owned_args_unpinned_enabled`].
#[inline]
pub fn door_pins_call_args(wanted: bool, callee_owns: bool, unpinned_on: bool) -> bool {
    wanted && !(callee_owns && unpinned_on)
}


/// Run a synchronous callee compile with the call's reference arguments
/// pinned (see [`CompileArgPins`]), and answer the compile's result together
/// with the argument buffer as it stands afterwards (`None`: unchanged).
///
/// For callers that hold NO `jit_thread_mut` borrow: the thread is borrowed
/// once to pin and once to release, never across `compile`.
///
/// # Safety
/// As [`jit_thread_mut`]: no other borrow of the current JIT thread may be
/// live at the call.
pub unsafe fn compile_with_ref_args_pinned<R>(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args: &[i64],
    compile: impl FnOnce() -> R,
) -> (R, Option<Vec<i64>>) {
    let pins =
        jit_thread_mut().map(|(thread, _guard)| CompileArgPins::pin_raw(vm, thread, info, args));
    let result = compile();
    let moved = match pins {
        Some(pins) => match jit_thread_mut() {
            Some((thread, _guard)) => pins.release_raw(thread, args),
            // The thread pointer is restored by every nested entry, so this
            // cannot happen; if it did, `pins` is dropped here and its `Drop`
            // truncates to the watermark (gen r5w4/jit8 -- it used to stay
            // until the next watermark release below it).
            None => None,
        },
        None => None,
    };
    (result, moved)
}

/// Call a compiled callee through `call` and finish the call through
/// [`route_implicit_exc_through_callee`], with the call's reference arguments
/// rooted across BOTH when `root_args`
/// (r11w4-rt-callee-rerun-reads-pre-call-argument-words). `None` when `call`
/// made no call (more arguments than the register tables cover).
///
/// A callee that returns the `i64::MIN` sentinel can be RE-RUN with this
/// call's arguments: from entry in the interpreter, or at its own handler.
/// `args` was read BEFORE the callee ran, and a compiled callee allocates and
/// reaches safepoints. While it runs, its own frame roots its parameters; once
/// it has returned, nothing roots `args`, so a moving collection leaves a word
/// that reads back as `null` (nothing there now) or as ANOTHER object
/// (something was allocated there). The re-run then throws an NPE the program
/// never had, or runs on the wrong object. `load_and_forward` cannot repair it
/// after the fact: ZGC's slide leaves no forwarding word. The window does not
/// close when the callee returns: the routing itself allocates (the implicit
/// throwable) and can run a deopt-frame rebuild before it reads the arguments,
/// so the pins stay until the routing is done and it reads the words back
/// through them ([`args_now`]).
///
/// Only a callee that declares an exception table can take a re-run, so each
/// caller passes that fact (memoized in its [`DispatchCache`] entry, or asked
/// once on a cold fill) and every other call pays nothing.
///
/// # Safety
/// As [`compile_with_ref_args_pinned`]: no `jit_thread_mut` borrow may be live
/// at the call. `receiver_class_id` as [`route_implicit_exc_through_callee`].
#[inline]
pub unsafe fn call_compiled_then_route(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args: &[i64],
    receiver_class_id: ClassId,
    root_args: bool,
    call: impl FnOnce() -> Option<i64>,
) -> Option<i64> {
    let pins = if root_args {
        jit_thread_mut().map(|(thread, _guard)| CompileArgPins::pin_raw(vm, thread, info, args))
    } else {
        None
    };
    let routed = call().map(|rc| {
        route_implicit_exc_through_callee(vm, info, args, receiver_class_id, rc, pins.as_ref())
    });
    if let Some(pins) = pins {
        // As `compile_with_ref_args_pinned`: every nested entry restores the
        // thread pointer, so this finds it; if it did not, `pins` is dropped
        // at the end of this block and its `Drop` truncates to the watermark.
        if let Some((thread, _guard)) = jit_thread_mut() {
            pins.release(thread);
        }
    }
    routed
}

/// `args` as it stands now under `pins` ([`CompileArgPins::current_raw`]);
/// `args` itself when nothing is pinned or nothing moved.
pub fn args_now<'a>(
    pins: Option<&CompileArgPins>,
    thread: &JvmThread,
    args: &'a [i64],
) -> std::borrow::Cow<'a, [i64]> {
    match pins.and_then(|pins| pins.current_raw(thread, args)) {
        Some(moved) => std::borrow::Cow::Owned(moved),
        None => std::borrow::Cow::Borrowed(args),
    }
}

/// Repair one JIT argument if the collector moved the object it names.
///
/// # The seventh plausibility filter in this file, and why it looks like none
///
/// The `raw >= 1 << 48` test below is 2.5.5 of
/// `docs/feature-designs/zgc-jit-load-barrier.md`: a hand-rolled plausibility
/// check that does not call `plausible_heap_pointer`, so it does not appear in
/// any grep for the idiom and was missed by that document's first draft. It
/// degrades to "skip forwarding" rather than to null, which is why it is not on
/// the silent-null list -- but a ZGC colored word has bit 63 set and would trip
/// it, so the argument would silently NOT be forwarded, with no diagnostic.
///
/// It is deliberately left alone. JIT arguments arrive already barriered: they
/// came from a caller's barriered load or from a fresh allocation, which
/// carries the allocating color by construction. That is an ASSUMPTION rather
/// than a proof, and it is the same class of assumption as J2, so it is
/// recorded here instead of being rediscovered.
///
/// # Why the ZGC load barrier is NOT plumbed through here
///
/// 2.5.5 suggests this as the natural home for the dispatch, because
/// `vm.mem.heap.load_and_forward` below is already this file's
/// backend-dispatching chokepoint for moving collectors. That reading does not
/// survive the signature: `load_and_forward` takes an `ObjectRef` -- a machine
/// pointer the caller has already built -- and therefore cannot accept a
/// colored word at all. It repairs a plain address the compacting slide moved
/// (its `Zgc` arm is a real `forwarded_after_slide` lookup, not a no-op); it
/// does not decode a color. The colored-word chokepoint has to take the SLOT,
/// which is what [`jit_load_ref_slot`] and `.agent-requests/A9-gc-barrier.txt`
/// are about.
#[inline]
pub fn forward_jit_arg_at(
    vm: &SharedVm,
    original: &[i64],
    replacement: &mut Option<Vec<i64>>,
    index: usize,
    raw: i64,
) {
    if raw == 0 || (raw as u64 & 0x7) != 0 || (raw as u64) >= (1u64 << 48) {
        return;
    }
    // SAFETY: the JIT calling convention guarantees that descriptor-declared
    // references are live object pointers at this boundary; the canonicality
    // checks above reject immediate/tagged values before constructing ObjectRef.
    let object = unsafe { ObjectRef::from_raw(raw as usize as *mut u8) };
    let forwarded = vm.mem.heap.load_and_forward(object).as_ptr() as i64;
    if forwarded != raw {
        let args = replacement.get_or_insert_with(|| original.to_vec());
        args[index] = forwarded;
    }
}

/// DIAGNOSTIC: crash-handler-safe read of the current JIT callee name.
/// Uses `try_with`/`try_borrow` so it never panics if the TLS is being
/// destroyed or the `RefCell` is already borrowed when a fault lands here.
/// Returns an empty string if unavailable. Only meaningful when
/// `CRATONVM_DBG_JIT_PUTFIELD=1` (that gate is what populates the cell).
pub fn current_jit_callee_for_crash() -> String {
    CURRENT_JIT_CALLEE
        .try_with(|c| c.try_borrow().map(|s| s.clone()).unwrap_or_default())
        .unwrap_or_default()
}

/// DIAGNOSTIC RAII guard: records `info` as the current callee, and on drop
/// restores the PREVIOUS value. This makes `current_jit_callee()` name the
/// method whose body is *currently executing inline* (the one containing a
/// faulting putfield), rather than a sub-call it dispatched and returned from.
pub struct JitCalleeGuard(pub String);
impl JitCalleeGuard {
    pub fn new(info: &JitInvokeInfo) -> Self {
        let prev = CURRENT_JIT_CALLEE.with(|c| {
            let mut s = c.borrow_mut();
            let prev = s.clone();
            s.clear();
            s.push_str(info.class_name);
            s.push('.');
            s.push_str(info.method_name);
            s.push_str(info.descriptor);
            prev
        });
        JitCalleeGuard(prev)
    }
}
impl Drop for JitCalleeGuard {
    fn drop(&mut self) {
        CURRENT_JIT_CALLEE.with(|c| {
            let mut s = c.borrow_mut();
            s.clear();
            s.push_str(&self.0);
        });
    }
}

/// Set the current thread's JvmThread pointer for JIT helper access.
/// Returns a [`JitThreadScope`] capturing the previously-stored pointer (and,
/// in debug builds, the suspended borrow level) so callers can restore both
/// later via [`restore_jit_thread`]. Suspending the borrow level here is what
/// makes a re-entrant JIT call (interpreter::execute inside
/// jit_invoke_dispatch) a *nested child reborrow* rather than a false-positive
/// *aliasing sibling* under the `jit_thread_mut` debug check.
///
/// # Safety contract
/// The caller must ensure that no other `&mut JvmThread` reference exists for the
/// duration of JIT execution. The pointer is only dereferenced inside JIT helpers
/// which execute on the same thread that set it.
pub fn set_jit_thread(thread: &mut JvmThread) -> JitThreadScope {
    // Shadow-stack precise roots (CRATONVM_SHADOW_STACK): ensure this thread's
    // shadow stack is allocated before any JIT code that may push to it runs.
    // Cheap `base != 0` check after the first entry; gated, no-op otherwise.
    // Done here (with a legitimate `&mut JvmThread`) rather than in the extern-C
    // `jit_get_current_thread` getter to avoid deriving an aliasing `&mut`.
    let mut saved_shadow_top: Option<usize> = None;
    if crate::jit::conservative_roots::shadow_stack_enabled() {
        thread.shadow_stack.ensure_allocated();
        // §3 (unwind safety): snapshot the boundary `top` watermark so
        // `restore_jit_thread` can reset it on exit, healing any push that an
        // abnormal JIT exit (exception/deopt skipping a method epilogue) left
        // unbalanced. Captured after `ensure_allocated` so `top` is valid.
        saved_shadow_top = Some(thread.shadow_stack.top);
        // Publish this thread's shadow-stack ADDRESS, plus the `base`/`end` of
        // its backing buffer, so a GC initiator that freezes this thread can
        // scan the window it cannot otherwise reach -- and can verify the
        // address still describes THIS buffer before trusting `top`.
        // Once per thread. See
        // `cratonvm_gc::gc_quiescence::publish_self_shadow_addr`.
        if crate::jit::conservative_roots::xt_peer_shadow_scan_enabled() {
            crate::jit::conservative_roots::publish_self_shadow_addr_once(
                &thread.shadow_stack as *const _ as usize,
                thread.shadow_stack.base,
                thread.shadow_stack.end,
            );
        }
        if if crate::runtime::env_cache::hot_lookup_cache() {
            crate::runtime::env_cache::dbg_shadow()        } else {
            false        } {
            use std::sync::atomic::{AtomicBool, Ordering};
            pub static ONCE: AtomicBool = AtomicBool::new(false);
            if !ONCE.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "[SHADOW] set_jit_thread: thread={:p} shadow base={:#x} top={:#x} end={:#x} ss_off={}",
                    thread as *mut JvmThread,
                    thread.shadow_stack.base,
                    thread.shadow_stack.top,
                    thread.shadow_stack.end,
                    JvmThread::shadow_stack_offset(),
                );
            }
        }
    }
    // Round 13 wave 4 (lane mega3, M13-3): the floor compiled inline-cache
    // stack checks read through the mirror published below. Rewritten on every
    // publish because a thread's `JvmThread` is not tied to one OS stack.
    //
    // NEVER with the suspended value `0`. `raise_jit_stack_overflow` builds its
    // error under `SuspendedStackFloor`, which stores `0` in the TLS cell, and
    // the constructor's Java re-enters compiled code through this function.
    // Copying that `0` into the word disarmed every compiled inline-cache check
    // of the thread for good: nothing rewrites the word when the suspension
    // ends, so the next unbounded recursion ran the 128 MiB `main-vm` stack
    // into its guard page (orchestrator, build w4c: `R13CrashIcRecursionSoe`,
    // `R13MegaIcRecursionMixed`, EXCEPTION_STACK_OVERFLOW). Keeping the armed
    // value is sound during a suspension too: a check that trips on it goes to
    // its exact side, which reads the TLS cell (`jit_native_stack_floor` in
    // single-pass, `jit_self_call_stack_guard` in the IR tier), sees the
    // suspension and lets the call proceed.
    let floor = current_thread_self_call_floor(SelfCallBudget::of_thread(thread));
    if floor != 0 {
        thread.jit_stack_floor = floor;
    }
    let prev_ptr = JIT_THREAD.with(|t| {
        let old = t.get();
        t.set(thread as *mut JvmThread);
        old
    });
    cratonvm_jit::x64::publish_jit_thread_mirror(thread as *mut JvmThread as usize);
    // Suspend any borrow held by an outer JIT level: the nested JIT call about
    // to run is a child reborrow of `thread`, not an aliasing sibling, so it
    // must start its own borrow level. The outer borrow is frozen on the call
    // stack and is provably unused until this nested call returns and
    // `restore_jit_thread` un-suspends it.
    #[cfg(debug_assertions)]
    let prev_borrow = suspend_jit_borrow();
    JitThreadScope {
        prev_ptr,
        saved_shadow_top,
        #[cfg(debug_assertions)]
        prev_borrow,
    }
}

/// Check if the JIT thread pointer is already set.
#[inline(always)]
pub fn is_jit_thread_set() -> bool {
    JIT_THREAD.with(|t| !t.get().is_null())
}

/// The `*mut JvmThread` installed for this OS thread, or null when none is.
///
/// Unlike [`jit_get_current_thread`] this does **no** JIT-boundary bookkeeping
/// (`note_jit_boundary`), so GC-side verification code can consult it without
/// perturbing the per-thread scan cache it is in the middle of using.
#[inline(always)]
pub fn current_jit_thread_ptr() -> *mut JvmThread {
    JIT_THREAD.with(|t| t.get())
}

/// Restore a previously saved JIT thread scope. Used to support re-entrant
/// JIT calls (e.g. JIT put() → jit_invoke_dispatch → interpreter::execute hash()
/// which may JIT-compile hash() and call set_jit_thread again). Re-installs the
/// prior thread pointer and un-suspends the outer level's debug borrow flag.
pub fn restore_jit_thread(scope: JitThreadScope) {
    // §3 (unwind safety): reset the shadow `top` to the watermark captured at the
    // matching `set_jit_thread`, healing any push that an abnormal JIT exit
    // (exception/deopt that skipped a method epilogue) left unbalanced. On a
    // normal return the per-method epilogues already restored `top`, so this is a
    // no-op (`set_top` is idempotent at the boundary value). `JIT_THREAD` still
    // names the thread whose JIT call just returned — it is reset to `prev_ptr`
    // just below — so it identifies the correct shadow stack to heal.
    if let Some(saved_top) = scope.saved_shadow_top {
        let cur = JIT_THREAD.with(|t| t.get());
        if !cur.is_null() {
            // SAFETY: execution is back on `cur`'s own thread and its JIT call has
            // returned, so no aliasing `&mut` to its shadow stack is live; the
            // backing buffer address is stable for the thread's lifetime, and
            // `set_top` clamps into `[base, end]` so a stale value can't widen the
            // scan range.
            unsafe { (*cur).shadow_stack.set_top(saved_top) };
        }
        report_shadow_overflow_once();
    }
    JIT_THREAD.with(|t| t.set(scope.prev_ptr));
    cratonvm_jit::x64::publish_jit_thread_mirror(scope.prev_ptr as usize);
    #[cfg(debug_assertions)]
    restore_jit_borrow(scope.prev_borrow);
}

/// One-shot warning when a JIT-emitted shadow-stack push has hit the `end`
/// guard and bailed. Checked at the JIT→Rust boundary (which is where the
/// watermark heal already runs) because the bail itself cannot call out — its
/// site has the callee's arguments staged in the ABI registers.
///
/// A bail is not a crash, but it is not free either: the safepoints that bailed
/// published no oops, so their `moving_young_coverage_complete` claim is not
/// backed at runtime while their calls are in flight, and every collection
/// that a bail may still overlap falls back to the non-moving sweep
/// (`conservative_roots::shadow_bail_refuses_moving`, gcd d2/f, per thread
/// since gcd d3/o; before d2/f nothing implemented that fallback). The printed
/// line below says "degraded" rather than "until X": the per-thread check
/// decides cycle by cycle. It always means some
/// compiled method leaks pushes, or a legitimately deep compiled recursion.
/// `CRATONVM_DBG_LOCAL_HANDLER_LOOKUP`, read once per process (nio21 wave 17:
/// it was a declared-flag read on every compiled local-handler lookup).
pub fn local_handler_lookup_dbg() -> bool {
    pub static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)}

pub fn report_shadow_overflow_once() {
    use std::sync::atomic::{AtomicBool, Ordering};
    pub static REPORTED: AtomicBool = AtomicBool::new(false);
    if REPORTED.load(Ordering::Relaxed) {
        return;
    }
    // nio21 wave 10 lane L5: HotSpot's stderr holds no such line, so it prints
    // under `CRATONVM_DBG=exit-census` or the diag switch it names
    // (`CRATONVM_SHADOW_OVERFLOW_DIAG`) only. The degradation it reports
    // (non-moving sweep) is unchanged either way.
    //
    // nio21 wave 17: the overflow counter (one relaxed atomic load) is asked
    // FIRST. This runs at every JIT->Rust boundary, and the two declared-flag
    // reads below each cost a UTF-8 decode plus two name hashes: the w16 host
    // `CRATONVM_DBG_FLAGREADS` census counted ~400 000 of each on the 200 000-op
    // `rlock` row. With no overflow they are now never read.
    let Some((count, label)) = cratonvm_jit::shadow_overflow_status() else {
        return;
    };
    if !(false
        || None::<std::ffi::OsString>.is_some())
    {
        return;
    }
    if REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "[JIT] shadow-stack overflow: {} push(es) bailed on the `end` guard{} \
         — a compiled method is leaking shadow pushes; GC precision has \
         degraded to the non-moving sweep. Set CRATONVM_SHADOW_OVERFLOW_DIAG=1 \
         to name the method.",
        count,
        match label.as_deref() {
            Some(m) => format!(" (last: {})", m),
            None => String::new(),
        }
    );
}

/// The loop-poll-word mirror a compiled-entry door publishes for its VM
/// (interpreter round i6 wave 119, lane L2; `CRATONVM_JIT_POLL_THREAD_WORD`,
/// default off; stage 3 of
/// `i24-L3-proposal-one-per-thread-poll-word-for-safepoints-and-frame-moves`).
///
/// Soundness: a compiled poll that reads the word instead of its VM's baked
/// flag sees that VM's pauses only if the word is counted by that VM's
/// barrier. [`Self::enter`] makes it so first: it asks the VM's barrier for
/// this OS thread's word (`GcBarrier::loop_poll_word_for_this_thread`, which
/// registers the word with that barrier unless it already is; a word stays in
/// every barrier list it joined until its thread ends, so a later
/// registration with another VM does not uncount it here). Only then is the
/// mirror set, and it is restored to the enclosing door's value (null outside
/// every door) when the scope ends, the `set_jit_thread` / `restore_jit_thread`
/// discipline. A thread that reaches compiled code by any path that is not
/// such a door finds the mirror null and polls the baked flag, as before.
/// A word counted by two barriers sends this VM's polls to the slow path
/// during the other VM's pauses too; the slow path then finds no pause of
/// its own VM and returns: a cost, never a missed pause.
pub struct PollWordMirrorScope {
    pub prev: usize,
    pub active: bool,
}

impl PollWordMirrorScope {
    /// Publish `shared`'s view of this thread's poll word. One cached-flag
    /// load when `CRATONVM_JIT_POLL_THREAD_WORD` is off.
    #[inline]
    pub fn enter(shared: &SharedVm) -> Self {
        if !cratonvm_jit::x64::jit_poll_thread_word_enabled() {
            return Self {
                prev: 0,
                active: false,
            };
        }
        let word = shared.mem.gc_barrier.loop_poll_word_for_this_thread();
        // SAFETY: the barrier answers this OS thread's word (or its own
        // always-raised one), valid for as long as this thread's door runs.
        let addr = unsafe { &*word }.word_addr();
        let prev = cratonvm_jit::x64::publish_loop_poll_word_mirror(addr);
        Self { prev, active: true }
    }
}

impl Drop for PollWordMirrorScope {
    fn drop(&mut self) {
        if self.active {
            let _ = cratonvm_jit::x64::publish_loop_poll_word_mirror(self.prev);
        }
    }
}

/// Install `thread` (null for none) as this OS thread's JIT thread pointer,
/// with its mirror, and hand back the previous one. Interpreter round i6 wave
/// 120, lane L6: the carrier-slice scope of `jdk_continuation::enter_special`
/// (`CRATONVM_VTHREAD_SLICE_ON_CARRIER`) clears it for the continuation's
/// slice and puts the carrier's back afterwards. Only between JIT calls (no
/// borrow of the installed thread is live): the debug borrow flag is left
/// alone.
pub fn replace_jit_thread_ptr(thread: *mut JvmThread) -> *mut JvmThread {
    let prev = JIT_THREAD.with(|t| t.replace(thread));
    cratonvm_jit::x64::publish_jit_thread_mirror(thread as usize);
    prev
}

/// Clear the JIT thread pointer after JIT execution completes.
pub fn clear_jit_thread() {
    JIT_THREAD.with(|t| t.set(std::ptr::null_mut()));
    cratonvm_jit::x64::publish_jit_thread_mirror(0);
}

/// The consolidated out-of-band JIT→interpreter signal block — see the
/// [`JIT_SIGNALS`] thread-local for field semantics.
///
/// Scalars only — the pending throwable lives on
/// [`JvmThread::jit_pending_exception`] so the collector can see and relocate
/// it (`jit-signals-root-gap.md`).
pub struct JitSignals {
    /// RBC.6 correctness fix — the bytecode pc of the `athrow` that produced
    /// the thread's `jit_pending_exception`, when statically known at
    /// JIT-compile time (`-1` = unknown,
    /// e.g. an exception propagated up from a dispatched callee, which this
    /// method has no bci for). Set ONLY by `jit_throw_exception` alongside the
    /// throwable itself; every other site that stashes a general exception (no
    /// known local throw site) leaves/resets this to `-1`. Consumed by
    /// `execute_jit_call` to give `route_jit_exception_through_method` a real
    /// `throw_pc` instead of the `usize::MAX`-means-unknown fallback, which
    /// cannot range-check a *typed* handler against the entry it actually
    /// belongs to — with 2+ exception-table entries whose catch types are in
    /// a subtype relationship (e.g. one entry catches `RuntimeException`, a
    /// LATER, unrelated entry catches `IllegalStateException`), the
    /// declaration-order type-only match picks whichever entry comes first
    /// regardless of which try-region actually threw, silently running the
    /// wrong handler. Confirmed via a two-sequential-try/catch differential
    /// repro (`AthrowCountBisect.twoThrowsSequential`,
    /// `vm/tests/jit_local_exception_handler_tests.rs`) before this fix.
    pub athrow_bci: Cell<i64>,
    /// Interpreter round i5 wave 102, lane L2: the identity of the method
    /// whose compiled exit stamped `athrow_bci` ([`method_identity_key`] of
    /// its name and descriptor), the token the exit packed beside the bci
    /// (`cratonvm_jit::implicit_null::throw_stamp_with_owner`). `0` = unknown
    /// (no token packed, or a Rust-side setter): the stamp is
    /// then trusted as before. Reset with `athrow_bci` by every general
    /// setter and the drain. See [`DrainedJitSignals::athrow_bci_owned_by`].
    pub athrow_owner: Cell<u64>,
    pub aioobe: Cell<Option<(i64, i64)>>,
    pub arithmetic: Cell<bool>,
    pub npe: Cell<bool>,
    pub npe_action: Cell<u8>,
    /// The COMPILED frames that were live when a JIT helper signalled the NPE.
    ///
    /// An implicit NPE in compiled code is not thrown where it happens: the
    /// helper sets `npe`, returns `i64::MIN`, compiled code returns to the
    /// interpreter, and only THEN is the `java/lang/NullPointerException`
    /// constructed. `fillInStackTrace` therefore runs on a stack the compiled
    /// frames have already left, and the trace loses every method between the
    /// throw site and the first interpreter frame — the whole point of the
    /// trace. Snapshot them while they are still on the stack; the drain hands
    /// them to the throwable. See
    /// `jit-compiled-frame-has-no-line-and-no-inlined-callees-FIXED-20260902.md`.
    ///
    /// `RefCell` rather than `Cell` because the payload is not `Copy`; it is
    /// only ever borrowed for the length of a `take`/`replace`, never across a
    /// call, so it cannot be re-entered.
    pub trap_frames:
        std::cell::RefCell<Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>>,
    pub deopt: Cell<bool>,
    /// What raised `deopt` last: a compiled body's frameless trap stub
    /// (`jit_uncommon_trap`), or anything else. See [`FramelessTrapSite`].
    pub frameless_trap: Cell<FramelessTrapSite>,
    /// gce e1/f: the pending exception ESCAPED the method named here
    /// ([`redispatch_escape_hash`]; `0` = none) after a not-entrant
    /// re-dispatch ran it through the VM's invoke path
    /// (`jit_not_entrant_entry_body`), which consults the method's exception
    /// table itself. So the call site's service must not search that table a
    /// second time. See [`take_redispatch_escape_for`]. Cleared by every
    /// pending-exception setter and by [`take_all_jit_signals`].
    pub redispatch_escape: Cell<u64>,
    /// nio21 wave 17, lane L5: an implicit-exception setter skipped its
    /// compiled-frame snapshot because the pad's local-handler probe follows
    /// (`cratonvm_jit::implicit_null::TRAP_SNAPSHOT_DEFER_BIT`). Taken by
    /// [`jit_local_handler_lookup`], which runs next with the same frames
    /// live and takes the snapshot itself unless the throw is served by the
    /// fast throw's preallocated instance. See [`DeferredTrapSnapshot`].
    pub trap_snapshot_deferred: Cell<DeferredTrapSnapshot>,
    /// JIT round 20 (lane throw): how many compiled activations per chain
    /// entry a trap snapshot walks (innermost first), as the bounded live
    /// capture walks them (`stackwalker::throwable_walk_activation_bound` of
    /// the VM's `-XX:MaxJavaStackTraceDepth`). `usize::MAX` (the whole chain,
    /// as before) until a door that knows the VM sets it
    /// ([`note_trap_snapshot_bound`]). A scalar, like every field here.
    pub trap_snapshot_bound: Cell<usize>,
}

/// The snapshot an implicit-exception setter deferred to the probe that
/// follows it (nio21 wave 17, lane L5; proposal L5W16-1): what to call to take
/// it exactly as the setter would have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeferredTrapSnapshot {
    /// Nothing deferred: the setter snapshotted (or the snapshot is off).
    None,
    /// [`snapshot_trap_frames_at_bci`] with this stamped bci (AIOOBE, `/ by zero`).
    AtBci(i64),
    /// [`snapshot_trap_frames`] with this inline null-check trap key (NPE).
    NpeKey(u32),
}

/// Record a deferred snapshot in place of taking one: the slot is cleared
/// exactly as the setter's own snapshot would have overwritten it. With the
/// snapshot switched off (`CRATONVM_JIT_NO_NPE_FRAME_SNAPSHOT=1`) nothing is
/// recorded and nothing is cleared, as the setter does then.
pub fn defer_trap_snapshot(deferred: DeferredTrapSnapshot) {
    if !trap_frame_snapshot_enabled() {
        return;
    }
    JIT_SIGNALS.with(|s| {
        *s.trap_frames.borrow_mut() = None;
        s.trap_snapshot_deferred.set(deferred);
    });
}

/// Take the deferred-snapshot record ([`JitSignals::trap_snapshot_deferred`]).
pub fn take_deferred_trap_snapshot() -> DeferredTrapSnapshot {
    JIT_SIGNALS.with(|s| s.trap_snapshot_deferred.replace(DeferredTrapSnapshot::None))
}

/// Take, now, the snapshot a setter deferred, into the signal block exactly
/// where the setter would have put it. Must run while the frames the setter
/// saw are still live: inside the probe's `jit_local_handler_lookup`, before
/// it returns to the pad.
///
/// The one difference from the setter's own walk: the probe has already
/// cleared this frame's safepoint-id slot to the no-map sentinel. The
/// innermost frame's bci and inline chain do not come from that slot when the
/// trap site is described (an NPE with a trap key, `apply_npe_trap_site`; a
/// stamped AIOOBE / `/ by zero` bci on a single-pass precise entry,
/// `apply_direct_throw_trap_bci`), which is every pad that sets the defer bit
/// in practice; otherwise the walk answers `-1` for that frame where the
/// setter would have answered the line of its last call, both of which are
/// the undescribed-site answer.
pub fn take_deferred_trap_snapshot_now(deferred: DeferredTrapSnapshot) {
    match deferred {
        DeferredTrapSnapshot::None => {}
        DeferredTrapSnapshot::AtBci(bci) => snapshot_trap_frames_at_bci(bci),
        DeferredTrapSnapshot::NpeKey(key) => snapshot_trap_frames(key),
    }
}


/// Which producer raised the thread's deopt flag last, for the replay rule of
/// a re-run after a BARE sentinel (round 13 wave 8, lane replay4;
/// `deopt_resume::frameless_rerun_is_exact`).
///
/// Every producer of the flag goes through [`set_jit_deopt_pending`], which
/// records [`Self::Helper`]; the frameless trap stub's helper
/// (`jit_uncommon_trap`) then overwrites it with [`Self::Stub`]. So while the
/// flag is set, this names its producer. Taking the flag leaves the record,
/// which is read only beside a set flag's outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FramelessTrapSite {
    /// Nothing recorded on this thread yet, or a stub with no compile id.
    Unknown,
    /// A helper raised the flag: an exception stub, a helper that could not
    /// build its throwable or found no JIT thread, a contained panic, a
    /// dropped frame. None of these is one of the body's frameless trap stubs.
    Helper,
    /// The frameless trap stub of the body with this compile id, at `bci`
    /// (interpreter bytecode). `exact` is that body's
    /// `frameless_traps_replay_exact`, read while it was trapping.
    ///
    /// `method` (round 13 wave 13, lane jitfix;
    /// `r13w12-replay6-callee-service-trusts-any-stubs-exact-stamp`): the
    /// trapping body's METHOD, as [`frameless_trap_method_hash`] of its
    /// label, `0` when the label did not parse. `exact` is a statement about
    /// re-running that method only, so a call-site service trusts it only
    /// for a callee with the same identity. Deliberately not the published
    /// body's compile id: the trap's own evict / recompile can replace that
    /// body before the service asks.
    Stub {
        compile_id: u32,
        bci: u32,
        exact: bool,
        method: u64,
    },
    /// A compiled callee's stashed frame that `try_resume_trapped_callee`
    /// dropped (its rebuild failed after a collection), judged by the replay
    /// rule at its own bci before it was dropped. Only the callee's call-site
    /// service may read it; any other sink answers the whole-body rule.
    Dropped { exact: bool },
    /// gce e1/f: an owned-arguments call site handed its callee's trap to the
    /// caller's caller, which re-runs the CALLER from entry
    /// ([`note_owned_args_handoff`]). The site admits ownership only where the
    /// caller's bytecode before the call commits nothing, so that re-run
    /// replays the callee's prefix alone, and `exact` is the callee replay's
    /// verdict. Any sink may read it: the next one to re-run is the caller's.
    /// Before, the sink judged the bare sentinel as the caller body's own
    /// frameless trap and could call the hand-off unsound and retire the
    /// caller.
    Handoff { exact: bool },
}

/// Record that the deopt flag just raised stands for a dropped callee frame
/// judged `exact` ([`FramelessTrapSite::Dropped`]; round 13 wave 8, lane
/// replay4). Call right after [`set_jit_deopt_pending`].
pub fn note_dropped_callee_frame(exact: bool) {
    JIT_SIGNALS.with(|s| s.frameless_trap.set(FramelessTrapSite::Dropped { exact }));
}

/// The method identity `FramelessTrapSite::Stub::method` carries: FNV-1a
/// over `class.method` and the descriptor, never `0` (which means "unknown").
/// The producer hashes the trapping body's `method_label`
/// ([`split_method_label`]); a call-site service hashes the callee it resolved
/// (its declaring class's name, the site's name and descriptor) -- the same
/// three strings the JIT cache is keyed on at both ends.
pub fn frameless_trap_method_hash(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let parts = [
        class_name.as_bytes(),
        ".".as_bytes(),
        method_name.as_bytes(),
        descriptor.as_bytes(),
    ];
    for part in parts {
        for &b in part {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h.max(1)
}

/// The producer of the thread's last deopt flag ([`FramelessTrapSite`]).
pub fn last_frameless_trap_site() -> FramelessTrapSite {
    JIT_SIGNALS.with(|s| s.frameless_trap.get())
}

/// One-shot snapshot-and-clear of EVERY out-of-band JIT signal, produced by
/// [`take_all_jit_signals`] in a single thread-local access. The
/// interpreter's post-JIT-return drain consumes this instead of six separate
/// `take_*` calls.
pub struct DrainedJitSignals {
    pub exception: Option<ObjectRef>,
    /// See `JitSignals::athrow_bci`. `-1` iff unknown (matches the field's
    /// own sentinel, so callers can pass it straight through as `usize::MAX`
    /// when negative without an extra branch).
    pub athrow_bci: i64,
    /// See `JitSignals::athrow_owner`; `0` = unknown.
    pub athrow_owner: u64,
    pub aioobe: Option<(i64, i64)>,
    pub arithmetic: bool,
    pub npe: bool,
    /// The compiled frames that were live when the helper signalled `npe`.
    /// See `JitSignals::trap_frames`; drained here so that no path
    /// can construct the NPE without also being handed the frames it is
    /// about to lose.
    pub trap_frames: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
    /// Drained alongside `npe` for hygiene (a stale action code must not
    /// outlive its NPE). Read by the two restash paths, which put it back with
    /// the flag and the frame snapshot — see [`restash_jit_pending_npe`].
    pub npe_action: u8,
    pub deopt: bool,
    /// See `JitSignals::redispatch_escape` (`0` = none). Interpreter round i5
    /// wave 103, lane L2: carried out of the drain (it used to be cleared
    /// unread) so an interpreter door can see that the throwable already left
    /// the method it entered through a not-entrant re-dispatch
    /// ([`Self::escaped_a_redispatch_of`]).
    pub redispatch_escape: u64,
}

/// Snapshot-and-clear ALL JIT signals: one thread-local access for the six
/// scalars, plus one field read on `thread` for the pending throwable.
/// Draining everything unconditionally is deliberate: a signal surviving into
/// the next unrelated JIT call was the recurring Round-8..11 leak-bug class,
/// and clearing a flag nobody set is free.
///
/// `thread` must be the thread whose JIT call just returned — the same one
/// that was installed via `set_jit_thread`. It is the drain's *only* way to
/// reach the throwable now that the reference is GC-rooted on the thread
/// rather than parked in TLS; passing a different thread silently drains an
/// empty slot and strands the exception on the real one.
#[inline]
pub fn take_all_jit_signals(thread: &mut JvmThread) -> DrainedJitSignals {
    let exception = thread.jit_pending_exception.take();
    JIT_SIGNALS.with(|s| {
        // gce e1/f: a call-site service that wants it takes it first
        // ([`take_redispatch_escape_for`]); drained here for hygiene, and
        // handed out (interpreter round i5 wave 103, L2) for the interpreter
        // doors ([`DrainedJitSignals::escaped_a_redispatch_of`]).
        let redispatch_escape = s.redispatch_escape.replace(0);
        // nio21 wave 17 L5: the probe's lookup takes it before any drain;
        // a record that outlived its trap must not defer the next one's.
        s.trap_snapshot_deferred.set(DeferredTrapSnapshot::None);
        DrainedJitSignals {
            redispatch_escape,
            exception,
            athrow_bci: s.athrow_bci.replace(-1),
            athrow_owner: s.athrow_owner.replace(0),
            aioobe: s.aioobe.take(),
            arithmetic: s.arithmetic.take(),
            npe: s.npe.take(),
            trap_frames: s.trap_frames.borrow_mut().take(),
            npe_action: s.npe_action.take(),
            deopt: s.deopt.take(),
        }
    })
}

/// Interpreter round i5 wave 102, lane L2: the method identity a throw-site
/// stamp carries ([`JitSignals::athrow_owner`]): the method's
/// `cratonvm_jit::implicit_null::method_stamp_token` of the body's declaring
/// class, name and descriptor (the class is in it because `run()V`-like
/// shapes recur across classes). Never `0`.
pub fn method_identity_key(class_name: &str, method_name: &str, descriptor: &str) -> u64 {
    u64::from(cratonvm_jit::implicit_null::method_stamp_token(
        class_name,
        method_name,
        descriptor,
    ))
}

/// [`method_identity_key`] of a `CompiledMethod::method_label`
/// (`"pkg/C.name:(desc)R"`), or `0` for a label that is not a method's (a
/// lambda adapter's `"lambda-adapter->0x.."`, an empty label).
pub fn method_identity_key_of_label(label: &str) -> u64 {
    u64::from(cratonvm_jit::implicit_null::method_stamp_token_of_label(
        label,
    ))
}

/// Split a compiled exit's stamp word (`cratonvm_jit::implicit_null::
/// throw_stamp_with_owner`) into the stamp and the stamping method's token
/// (`0` = unknown). The token is baked at compile time from the method's
/// class, name and descriptor (build w102 hand-back: not a compile id, which made codegen
/// non-deterministic), so nothing is resolved here.
pub fn split_throw_stamp_word(raw: i64) -> (i64, u64) {
    let (stamp, token) = cratonvm_jit::implicit_null::split_throw_stamp_owner(raw);
    (stamp, u64::from(token))
}

impl DrainedJitSignals {
    /// Interpreter round i5 wave 102, lane L2 (page
    /// `i101-L2-pc-unknown-handler-search-matches-typed-rows-outside-their-range`):
    /// `athrow_bci` as a throw pc of the method `class_name.method_name
    /// descriptor` (the declaring class of the drained body), or `-1` when the
    /// stamp names ANOTHER method. Before, a
    /// foreign stamp that happened to be an instruction boundary of this
    /// method was range-checked as this method's own pc; the drains now see
    /// "unknown" for it, which is what it is. A stamp of unknown owner is
    /// answered as before.
    pub fn athrow_bci_owned_by(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> i64 {
        self.athrow_bci_owned_by_key(method_identity_key(class_name, method_name, descriptor))
    }

    /// [`Self::athrow_bci_owned_by`] for the drain that holds the body it
    /// entered rather than the method's name: keyed by the body's label. A
    /// label that names no method (a lambda adapter) checks nothing.
    pub fn athrow_bci_owned_by_label(&self, label: &str) -> i64 {
        match method_identity_key_of_label(label) {
            0 => self.athrow_bci,
            key => throw_stamp_owned_by_named(self.athrow_bci, self.athrow_owner, key, label),
        }
    }

    pub fn athrow_bci_owned_by_key(&self, key: u64) -> i64 {
        throw_stamp_owned_by(self.athrow_bci, self.athrow_owner, key)
    }

    /// Interpreter round i5 wave 103, lane L2 (page
    /// `i101-L2-pc-unknown-handler-search-matches-typed-rows-outside-their-range`):
    /// did the pending throwable already LEAVE the method `method_name
    /// descriptor` through a not-entrant re-dispatch, so that no table of
    /// that method may be searched for it again?
    ///
    /// A door checks that the body is entrant BEFORE it enters, and the
    /// background tier-up can retire the body between that check and the
    /// call. The entry then runs the not-entrant stub, whose first pass answers
    /// the call through the VM's invoke path (`jit_not_entrant_entry_body`):
    /// the method ran there, its handlers were offered the exception at the
    /// real throw pc, and the throwable that escaped is re-posted with NO
    /// stamp. The door then read "pc unknown", and the pc-unknown search
    /// matched a typed row by exception class alone: one swallowed exception
    /// per supersede (`test_callee_that_cannot_catch_propagates_instead_of_re_running`,
    /// 19997 of 20000 under load). The call-site services already honour the
    /// record (`take_redispatch_escape_for`); the doors drained it unread.
    ///
    /// Only an UNSTAMPED throwable counts: the re-post sets no stamp, and the
    /// stub returns straight to the door, so a stamp means a compiled frame of
    /// some body ran its own exit after the record was written (a stale record
    /// left by a nested activation of the same method).
    pub fn escaped_a_redispatch_of(&self, method_name: &str, descriptor: &str) -> bool {
        self.exception.is_some()
            && self.athrow_bci < 0
            && self.redispatch_escape != 0
            && not_entrant_escape_enabled()
            && self.redispatch_escape == redispatch_escape_hash(method_name, descriptor)
    }
}

/// The body of [`DrainedJitSignals::athrow_bci_owned_by`] on the two fields,
/// for a drain that has already moved the throwable out of its record. `key`
/// is the drained method's [`method_identity_key`].
pub fn throw_stamp_owned_by(athrow_bci: i64, athrow_owner: u64, key: u64) -> i64 {
    throw_stamp_owned_by_named(athrow_bci, athrow_owner, key, "")
}

/// [`throw_stamp_owned_by`] for a drain that knows the drained body's label,
/// which the census line then names (interpreter round i5 wave 104, lane L2).
pub fn throw_stamp_owned_by_named(athrow_bci: i64, athrow_owner: u64, key: u64, label: &str) -> i64 {
    if athrow_bci < 0 || athrow_owner == 0 || key == 0 || athrow_owner == key {
        return athrow_bci;
    }
    if crate::runtime::env_cache::dbg_jitc() {
        // Interpreter round i5 wave 104, lane L2: `JITC-PC-CENSUS` is the one
        // tag both close-criterion lines of page `i101-L2-pc-unknown-...` carry
        // (this one and `pc-unknown typed match`), so a battery log is counted
        // with a single `grep -c JITC-PC-CENSUS`.
        eprintln!(
            "[cratonvm-jitc] JITC-PC-CENSUS foreign throw stamp refused: bci={athrow_bci} owner={athrow_owner:#x} drained={key:#x} label={label}{}",
            jitc_census_backtrace()
        );
    }
    -1
}

/// Interpreter round i5 wave 104, lane L2: the Rust call path of a
/// `JITC-PC-CENSUS` line, so one run names the drain (and so the producer
/// whose stamp leaked). Opt-in through the standard library's own
/// `RUST_LIB_BACKTRACE=1` (or `RUST_BACKTRACE=1`); empty otherwise. Only
/// ever called under `CRATONVM_DBG_JITC`.
pub fn jitc_census_backtrace() -> String {
    let bt = std::backtrace::Backtrace::capture();
    if bt.status() == std::backtrace::BacktraceStatus::Captured {
        format!("\n{bt}")
    } else {
        String::new()
    }
}

/// gce e1/f: the identity [`JitSignals::redispatch_escape`] records -- the
/// method's name and descriptor, which a not-entrant record and the call
/// site that reached it spell identically (the class a site names may be a
/// subclass of the declaring one). Never `0`.
pub fn redispatch_escape_hash(method_name: &str, descriptor: &str) -> u64 {
    frameless_trap_method_hash("", method_name, descriptor)
}

/// `CRATONVM_JIT_NOT_ENTRANT_ESCAPE` -- default ON, `=0` restores the old
/// behaviour (the call site's service searches the re-dispatched method's
/// exception table again). Read on the throw path only, no static.
pub fn not_entrant_escape_enabled() -> bool {
    true
}

/// gce e1/f (`gcd-d10f-not-entrant-redispatch-exception-reruns-the-callee-handler-DONE-20260928.md`):
/// the not-entrant re-dispatch returned `rc`; when that is the sentinel with a
/// throwable pending, the throwable has already left the method (the VM's
/// invoke path ran its handlers), so record that for the call site. Returns
/// `rc`.
pub fn note_redispatch_escape(thread: &JvmThread, info: &JitInvokeInfo, rc: i64) -> i64 {
    if rc == i64::MIN && thread.jit_pending_exception.is_some() && not_entrant_escape_enabled() {
        let h = redispatch_escape_hash(info.method_name, info.descriptor);
        JIT_SIGNALS.with(|s| s.redispatch_escape.set(h));
    }
    rc
}

/// `CRATONVM_JIT_DISPATCH_ERROR_ESCAPE_RECORD` (interpreter round i5 wave 106,
/// lane L2; parity fix, default ON, `=0` records only the not-entrant
/// re-dispatch as before). Read on the throw path only.
pub fn dispatch_error_escape_record_enabled() -> bool {
    true
}

/// Interpreter round i5 wave 106, lane L2 (page
/// `i101-L2-pc-unknown-handler-search-matches-typed-rows-outside-their-range`):
/// [`handle_jit_dispatch_error`] has just re-posted a throwable for the call of
/// `info` (`pending`: it stashed one). Every such re-post is a throwable that
/// already LEFT `info`'s method: the VM ran the method's bytecode (a trapped
/// frame resumed, a handler resumed in an interpreted frame, a re-run from
/// entry, a not-entrant re-dispatch, the by-name invoke path), whose unwinder
/// searched its table at the real throw pc; or the call failed before the
/// method was entered (a class-initialization or linkage error raised in the
/// dispatch helper), where the method's handlers do not apply at all. The
/// re-post carries no stamp, so the next consumer used to read "pc unknown":
/// the w105 battery's 115,360 `JITC-UNSTAMPED-ESCAPE` lines
/// (`L1W93XPair.rethrow`). Record it, the same record the not-entrant stub
/// writes ([`note_redispatch_escape`]): the call-site services
/// ([`take_redispatch_escape_for`]) and the interpreter doors
/// ([`DrainedJitSignals::escaped_a_redispatch_of`]) then propagate without a
/// search. Its exact-pc equivalent is the stamp `OutsideAllRanges`: the throw is
/// at a pc of the method no row of it catches, which is what an escape is.
///
/// A compiled exit that stamps afterwards retires the record
/// ([`jit_set_throw_bci`]), so it can only speak for the activation it was
/// written for (a self-recursive caller's own pad re-stamps before its own
/// caller's service reads anything).
pub fn note_dispatch_error_escape(pending: bool, method_name: &str, descriptor: &str) {
    if pending && not_entrant_escape_enabled() && dispatch_error_escape_record_enabled() {
        let h = redispatch_escape_hash(method_name, descriptor);
        JIT_SIGNALS.with(|s| s.redispatch_escape.set(h));
    }
}

/// gce e1/f: take the escape record ([`JitSignals::redispatch_escape`]) and
/// say whether it names `info`'s method, i.e. whether the pending throwable
/// already left the callee this site called. Taken, so it can speak for one
/// site only; a record naming another method is dropped (it was stale).
pub fn take_redispatch_escape_for(info: &JitInvokeInfo) -> bool {
    redispatch_escape_record_names(
        take_redispatch_escape_record(),
        info.method_name,
        info.descriptor,
    )
}

/// The raw escape record ([`JitSignals::redispatch_escape`], `0` = none),
/// taken. For a service that must ask about TWO methods with one record:
/// the MIC door of a SAM call, whose `info` names the SAM (`call`, `apply`)
/// while the not-entrant record names the body that was re-dispatched -- the
/// lambda IMPL (interpreter round i5 wave 104, lane L2).
pub fn take_redispatch_escape_record() -> u64 {
    JIT_SIGNALS.with(|s| s.redispatch_escape.replace(0))
}

/// Does `record` (from [`take_redispatch_escape_record`]) name the method
/// `method_name descriptor`?
pub fn redispatch_escape_record_names(record: u64, method_name: &str, descriptor: &str) -> bool {
    record != 0 && record == redispatch_escape_hash(method_name, descriptor)
}

/// Kill switch for the compiled-frame snapshot taken when a JIT helper signals
/// an implicit NPE. Default ON; `CRATONVM_JIT_NO_NPE_FRAME_SNAPSHOT=1` restores
/// the historical (frame-losing) trace, so the difference is an A/B inside one
/// binary rather than a comparison across two builds.
pub fn trap_frame_snapshot_enabled() -> bool {
    pub static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || !false)
}

/// Snapshot the live compiled frames for a JIT-signalled NPE.
///
/// Called from the two `set_jit_pending_npe*` setters, i.e. from inside the
/// helper, with every compiled frame between the throw site and the
/// interpreter still on the stack. Cheap by construction: this records the
/// same small structs the GC root walk already builds, and does no class-store
/// lookup, no string formatting and takes no lock.
pub fn snapshot_trap_frames(trap_key: u32) {
    if !trap_frame_snapshot_enabled() {
        return;
    }
    let mut frames = trap_snapshot_walk();
    // The one frame the walk cannot put a line on is the one that raised: an
    // inline null check publishes no safepoint id. `trap_key` is the site id its
    // cold trampoline passed in, and this is the only place both the key and
    // the frames exist at once.
    crate::jit::conservative_roots::apply_npe_trap_site(&mut frames, trap_key);
    JIT_SIGNALS.with(|s| {
        *s.trap_frames.borrow_mut() = (!frames.is_empty()).then_some(frames);
    });
}

/// [`snapshot_trap_frames`] for the two DIRECT-THROW helpers
/// (`jit_throw_arithmetic`, `jit_throw_aioobe`), whose single-pass stub knows
/// the trapping bci exactly: `stamped_bci` is that bci, or `-1` for none.
///
/// A zero-divisor guard or a bounds check publishes no safepoint id, so the
/// walk cannot place the frame that raised (`-1`, or the line of the last call
/// it made); `apply_direct_throw_trap_bci` puts the stub's bci on it. Page
/// `r11w14-orch-implicit-exception-frame-reports-line-minus-one`.
pub fn snapshot_trap_frames_at_bci(stamped_bci: i64) {
    if !trap_frame_snapshot_enabled() {
        return;
    }
    let mut frames = trap_snapshot_walk();
    crate::jit::conservative_roots::apply_direct_throw_trap_bci(&mut frames, stamped_bci);
    JIT_SIGNALS.with(|s| {
        *s.trap_frames.borrow_mut() = (!frames.is_empty()).then_some(frames);
    });
}

/// JIT round 20 (lane throw,
/// `jr20-deep-stack-throws-walk-the-whole-compiled-chain-and-compiled-only-throw-loops-never-take-the-trace-gc-request-FIXED-20261012.md`):
/// the compiled-frame walk of a trap snapshot. It used to read the WHOLE chain
/// for every implicit trap, so a recursion 10-20k compiled levels deep that
/// catches one exception per level paid O(depth^2) frame visits (and the
/// capture laid every snapshot frame out before the 1024 cap) where HotSpot
/// stops at `MaxJavaStackTraceDepth`. Bounded to the innermost
/// [`JitSignals::trap_snapshot_bound`] activations of each chain entry -- the
/// bound the live capture walk already uses for the same cap, so a capture
/// that takes the snapshot (stand-in or splice, both capped) keeps the same
/// frames: an entry is cut only past `bound` (>= 4097) activations, outside
/// the kept innermost `cap` frames.
///
/// The bound is per thread, set by the doors that know the VM; on a thread
/// that alternates between VMs with different caps, the last door's bound
/// applies (the cut can then drop frames only past the smaller VM's bound).
pub fn trap_snapshot_walk() -> Vec<crate::jit::conservative_roots::ActiveCompiledFrame> {
    let bound = JIT_SIGNALS.with(|s| s.trap_snapshot_bound.get());
    if bound == usize::MAX {
        return crate::jit::conservative_roots::active_compiled_frames();
    }
    crate::jit::conservative_roots::active_compiled_frames_bounded(0, bound)
}

/// JIT round 20 (lane throw): set this thread's trap-snapshot walk bound
/// ([`trap_snapshot_walk`]) from `vm`'s `-XX:MaxJavaStackTraceDepth`.
/// `CRATONVM_JIT_TRAP_SNAPSHOT_BOUNDED=0` keeps the whole-chain walk.
pub fn note_trap_snapshot_bound(vm: &SharedVm) {
    let bound = if trap_snapshot_bounded_enabled() {
        crate::runtime::stackwalker::throwable_walk_activation_bound(
            vm.config.max_java_stack_trace_depth,
        )
    } else {
        usize::MAX
    };
    JIT_SIGNALS.with(|s| s.trap_snapshot_bound.set(bound));
}

/// `CRATONVM_JIT_TRAP_SNAPSHOT_BOUNDED` -- default ON (JIT round 20, lane
/// throw), `=0` walks the whole compiled chain per trap snapshot, as before.
/// Read only where a door sets the bound, not per walk.
///
/// JIT round 20 wave 18 (lane xThrow, THROW20-5): those doors are two per
/// compiled implicit catch (the lookup and the construction), so the switch
/// was a flag-table read (name-set probe, override table) twice per throw.
/// Under `CRATONVM_JIT_IMPLICIT_THROW_LEAN_BUILD` (default ON) it is the value
/// latched with the other throw-path switches; `=0` reads it per call again.
pub fn trap_snapshot_bounded_enabled() -> bool {
    let switches = crate::runtime::exceptions::throw1_switches();
    if switches.implicit_throw_lean_build {
        return switches.trap_snapshot_bounded;
    }
    true
}

/// Take the compiled frames snapshotted for a pending JIT NPE, if any.
///
/// The drain calls this beside [`take_jit_pending_npe`]. Always a `take`: a
/// snapshot that outlived its NPE would be attached to an unrelated throwable,
/// and a trace that is confidently wrong is worse than one that is short.
pub fn take_jit_pending_trap_frames(
) -> Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>> {
    JIT_SIGNALS.with(|s| s.trap_frames.borrow_mut().take())
}

/// Store a pending Java exception from JIT dispatch. Called when
/// `jit_invoke_dispatch` encounters an `ExceptionThrown` error.
///
/// Always resets `athrow_bci` to `-1` (unknown) — this is the general,
/// origin-agnostic setter (a dispatched callee threw, or a re-stash), never
/// the direct-local-athrow path. Only `jit_throw_exception`'s dedicated
/// `set_jit_pending_exception_with_bci` may set a real bci, and only for the
/// exception it is stashing in that same call.
/// The throwable is stored on `thread` (not in `JIT_SIGNALS`) so that a
/// collection occurring between this stash and the interpreter's drain both
/// keeps it alive and rewrites it — see [`JvmThread::jit_pending_exception`].
pub fn set_jit_pending_exception(thread: &mut JvmThread, exc: ObjectRef) {
    thread.jit_pending_exception = Some(exc);
    JIT_SIGNALS.with(|s| {
        s.athrow_bci.set(-1);
        s.athrow_owner.set(0);
        // gce e1/f: a new (or re-stashed) throwable carries no escape record.
        s.redispatch_escape.set(0);
    });
}

/// RBC.6 correctness fix — sibling of `set_jit_pending_exception` for the ONE
/// call site (`jit_throw_exception`) that knows the exact bytecode pc of the
/// `athrow` producing this exception at JIT-compile time. See
/// `JitSignals::athrow_bci` for why this matters (typed-handler routing
/// correctness with 2+ exception-table entries when `throw_pc` would
/// otherwise be `usize::MAX`).
pub fn set_jit_pending_exception_with_bci(thread: &mut JvmThread, exc: ObjectRef, bci: i64) {
    set_jit_pending_exception_with_stamp(thread, exc, bci, 0);
}

/// [`set_jit_pending_exception_with_bci`] with the stamping method's identity
/// (`JitSignals::athrow_owner`, `0` = unknown). Interpreter round i5 wave 102,
/// lane L2.
pub fn set_jit_pending_exception_with_stamp(
    thread: &mut JvmThread,
    exc: ObjectRef,
    bci: i64,
    owner: u64,
) {
    thread.jit_pending_exception = Some(exc);
    JIT_SIGNALS.with(|s| {
        s.athrow_bci.set(bci);
        s.athrow_owner.set(owner);
        // gce e1/f: as `set_jit_pending_exception`.
        s.redispatch_escape.set(0);
    });
}

/// Round-9 vm CRIT fix (audit `round9-vm.md` CRIT-2): re-stash a previously
/// taken pending Java exception. Used by `try_osr` when the OSR return path
/// drained the flag but cannot return an error from its own signature — the
/// exception must be re-posted so the interpreter dispatch loop drains it on
/// the next iteration via `take_jit_pending_exception`. Crate-pub because
/// only the OSR entry path should use it; ordinary JIT helpers set the flag
/// directly via the private `set_jit_pending_exception` above.
///
/// Deliberately loses any `athrow_bci` the exception may have carried before
/// being taken (this is the general re-stash path, not the direct-athrow
/// one) — always falls back to the pre-existing `usize::MAX`-means-unknown
/// behavior for the re-stashed exception, never a regression, just not the
/// newly-precise case.
pub fn stash_jit_pending_exception(thread: &mut JvmThread, exc: ObjectRef) {
    set_jit_pending_exception(thread, exc);
}

/// Stash a `java.lang.InternalError` naming a JIT runtime helper whose body
/// panicked, for [`crate::jit::helper_guard::OnPanic::Throw`].
///
/// Returns whether a throwable was stashed. `vm_ptr` is the helper's own
/// `SharedVm` argument, or `0` for a helper that takes none, in which case the
/// process VM is used when exactly one is published. Nothing is stashed when no
/// JIT thread is installed or the throwable cannot be built, and the guard then
/// returns the bare sentinel: the same fallback [`jit_alloc_oom`] has.
///
/// Replaces an exception that was already pending. The helper that panicked did
/// not finish whatever it was doing with that exception, and the panic is the
/// report that has to reach Java.
#[cold]
pub fn stash_jit_helper_panic(vm_ptr: i64, helper: &'static str, detail: &str) -> bool {
    // SAFETY: called only from `helper_guard::contain` on the thread that ran
    // the guarded helper, after the helper's body has fully unwound, so every
    // `JitThreadGuard` that body took has already been dropped.
    let Some((thread, _guard)) = (unsafe { jit_thread_mut() }) else {
        return false;
    };
    let process_vm = if vm_ptr == 0 {
        crate::native::jni::process_vm_strict()
    } else {
        None
    };
    let vm: &SharedVm = match process_vm.as_deref() {
        Some(vm) => vm,
        // SAFETY: a non-zero `vm_ptr` is the `SharedVm` pointer compiled code
        // handed the guarded helper, under that helper's own caller contract.
        None if vm_ptr != 0 => unsafe { &*(vm_ptr as *const SharedVm) },
        None => return false,
    };
    let message = format!("JIT runtime helper {helper} panicked: {detail}");
    match crate::runtime::exceptions::create_exception_object(
        vm,
        thread,
        "java/lang/InternalError",
        Some(&message),
    ) {
        Ok(exc) => {
            set_jit_pending_exception(thread, exc);
            true
        }
        Err(_) => false,
    }
}

/// Forget the `athrow` bci carried by the pending exception, keeping the
/// exception itself.
///
/// Called when a dispatch helper hands a CALLEE's pending exception back to its
/// compiled caller: from that point the bci names a method that is no longer on
/// the stack, and the caller's drain would range-check its own exception table
/// against it. See `JitSignals::athrow_bci`.
pub fn clear_jit_athrow_bci() {
    JIT_SIGNALS.with(|s| {
        s.athrow_bci.set(-1);
        s.athrow_owner.set(0);
    });
}

/// [`take_jit_deopt_pending`] and [`clear_jit_athrow_bci`] in one
/// `JIT_SIGNALS` access: the entry retire of an interpreter-to-compiled door
/// under `CRATONVM_JIT_DOOR_THREAD_BLOCK` (t1 wave 5, lane S). The same three
/// cells end in the same state.
#[inline]
pub fn retire_door_entry_signals() {
    JIT_SIGNALS.with(|s| {
        s.deopt.set(false);
        s.athrow_bci.set(-1);
        s.athrow_owner.set(0);
    });
}

/// Read the `athrow` bci without consuming it.
///
/// Exists for the one consumer that must see the bci BEFORE
/// [`clear_jit_athrow_bci`] runs: `route_implicit_exc_through_callee` routes
/// through the CALLEE's own exception table, and the stamped bci is that
/// callee's own throw site — precisely the pc that lookup needs. See the
/// capture at that function's entry.
pub fn peek_jit_athrow_bci() -> i64 {
    JIT_SIGNALS.with(|s| s.athrow_bci.get())
}

/// Round-9 vm CRIT fix (audit `round9-vm.md` CRIT-2): re-stash a previously
/// taken pending-NPE flag. See `stash_jit_pending_exception` for the OSR
/// drain-without-route rationale.
///
/// JIT round 20 wave 7 (lane fuzzC): its last caller, `jit_throw_exception`'s
/// null arm, now raises [`set_jit_pending_npe_athrow`]; kept for the docs that
/// name it as the bare-signal setter.
#[allow(dead_code)]
pub fn stash_jit_pending_npe() {
    set_jit_pending_npe();
}

/// Raise the pending-NPE flag WITHOUT taking a compiled-frame snapshot and
/// WITHOUT touching the JEP-358 action code.
///
/// For the one shape [`stash_jit_pending_npe`] is wrong for: a door that
/// drained the flag with [`take_jit_pending_npe`] and is putting it back. That
/// take leaves `trap_frames` untouched, so the snapshot from the trap
/// is still there and is still the right one; taking another would overwrite it
/// with a stack the raising frame has already left.
///
/// The action cell is left alone for exactly the same reason, and it used to be
/// ZEROED here. That take does not consume the action either, so the code
/// sitting in the cell is this NPE's own — clearing it threw away the one thing
/// that tells the eventual drain which opcode trapped, and an `invoke` on a
/// null receiver that took a round trip through a door which declined to
/// service it came out unmessaged. See
/// `runtime::interpreter::jit_npe_message::jit_npe_message`, which reads it.
pub fn set_jit_pending_npe_flag_only() {
    JIT_SIGNALS.with(|s| s.npe.set(true));
}

/// Put back an NPE that [`take_all_jit_signals`] drained WHOLE -- flag, JEP-358
/// action code and the compiled-frame snapshot -- exactly as it was found.
///
/// The action code was previously dropped by every restash (the setter resets
/// it to `0`), so an implicit NPE that took a round trip through a door which
/// declined to service it lost its "Cannot load from int array" message. Both
/// halves travel together because both describe the same trap.
pub fn restash_jit_pending_npe(
    action: u8,
    frames: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
) {
    JIT_SIGNALS.with(|s| {
        s.npe.set(true);
        s.npe_action.set(action);
        *s.trap_frames.borrow_mut() = frames;
    });
}

/// Put back a div-by-zero that [`take_all_jit_signals`] drained WHOLE — flag
/// AND compiled-frame snapshot — exactly as it was found.
///
/// The sibling of [`restash_jit_pending_npe`], and it did not exist until
/// 2026-09-05: both whole-drain restores called the bare
/// [`stash_jit_pending_arithmetic`], which puts the flag back and lets
/// `DrainedJitSignals` drop the snapshot on the floor. The next door then
/// constructed the `ArithmeticException` with nothing to attach, and the
/// throwable kept the empty trace `fillInStackTrace` had just built — the
/// exact defect `restash_jit_pending_npe`'s own doc describes for its half,
/// one signal kind over.
///
/// Do NOT re-sample here. `active_compiled_frames()` at a restore point
/// describes a shallower stack than the trap did, which is the mistake
/// [`restash_implicit_signal`]'s NPE arm is written to avoid; the frames belong
/// to the trap, not to the drain that happened to pass them through.
pub fn restash_jit_pending_arithmetic(
    frames: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
) {
    JIT_SIGNALS.with(|s| {
        s.arithmetic.set(true);
        *s.trap_frames.borrow_mut() = frames;
    });
}

/// Put back an AIOOBE that [`take_all_jit_signals`] drained WHOLE — payload
/// AND compiled-frame snapshot — exactly as it was found.
///
/// The third of the family, after [`restash_jit_pending_npe`] and
/// [`restash_jit_pending_arithmetic`], and the same rule as both: do NOT
/// re-sample. `active_compiled_frames()` at a restore point describes a
/// shallower stack than the trap did; the frames belong to the trap, not to the
/// drain that happened to pass them through.
pub fn restash_jit_pending_aioobe(
    index: i64,
    length: i64,
    frames: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
) {
    JIT_SIGNALS.with(|s| {
        s.aioobe.set(Some((index, length)));
        *s.trap_frames.borrow_mut() = frames;
    });
}

/// Round-9 vm CRIT fix (audit `round9-vm.md` CRIT-2): re-stash a previously
/// taken pending-AIOOBE payload. See `stash_jit_pending_exception` for the
/// OSR drain-without-route rationale.
///
/// FLAG ONLY, and that is now a distinction that matters: a door which drained
/// the payload with [`take_jit_pending_aioobe`] left the snapshot where the
/// helper put it and must use this, while a door which drained everything with
/// [`take_all_jit_signals`] is holding the snapshot and must use
/// [`restash_jit_pending_aioobe`] instead.
pub fn stash_jit_pending_aioobe(index: i64, length: i64) {
    JIT_SIGNALS.with(|s| s.aioobe.set(Some((index, length))));
}

/// Take (consume) any pending Java exception set by JIT dispatch.
/// Returns `Some(ObjectRef)` if an exception was pending, `None` otherwise.
///
/// `thread` must be the thread whose JIT call stashed it — see
/// [`take_all_jit_signals`]. The returned `ObjectRef` is the *post-move*
/// address if a collection intervened, because the slot it comes out of is
/// rooted and remapped (`memory/roots.rs` §10, `memory/gc.rs` §10).
pub fn take_jit_pending_exception(thread: &mut JvmThread) -> Option<ObjectRef> {
    let exc = thread.jit_pending_exception.take();
    if exc.is_some() {
        drain_superseded_implicit_signals();
    }
    exc
}

/// Drop the implicit-trap flags when a real exception is being delivered.
///
/// An implicit signal (`npe`, `aioobe`, `arithmetic`) is a REQUEST for a
/// throwable, not a throwable. Once another exception is in flight, that request
/// can never be granted: the frame whose trap raised it is unwinding, and no
/// door downstream owns the flag. Leaving it set is not inert -- the next
/// unrelated JIT call drains it and builds a fresh exception at a site that
/// never faulted.
///
/// The shape that found this (2026-09-03, see
/// `internal/fixed-bugs/jit-superseded-implicit-npe-leak-FIXED-20260903.md`):
/// the lambda direct arm finishes a deopted body in the interpreter and returns
/// zero with the real NPE parked in `jit_pending_exception`, exactly as its
/// contract says. Compiled code then evaluates the second operand of the same
/// expression before its post-invoke guard fires, dereferences the SAME null and
/// raises a second trap. The first exception is delivered and caught; the second
/// flag survives two iterations and surfaces as a `NullPointerException` for a
/// receiver that was never null.
///
/// This is the same rule [`take_all_jit_signals`] already applies by taking
/// everything at once -- stated for the other consumption point, so the two
/// cannot disagree about whether a signal outlives the exception that overtook
/// it. The deopt flag is deliberately NOT dropped: it describes the compiled
/// frame's fate, which an exception does not settle.
pub fn drain_superseded_implicit_signals() {
    let npe = take_jit_pending_npe();
    if npe {
        let _ = take_jit_pending_npe_action();
    }
    let aioobe = take_jit_pending_aioobe().is_some();
    let arithmetic = take_jit_pending_arithmetic();
    // The snapshot was taken for a raise that will never happen; a later drain
    // would attach frames the raising code has long since left. All three
    // signals carry one now (the NPE since 2026-09-02, the div-by-zero and the
    // AIOOBE since 2026-09-05), so all three have to drop it — dropping it only
    // for the NPE would leave a superseded array trap's frames in the slot for
    // whatever raises next.
    if npe || aioobe || arithmetic {
        let _ = take_jit_pending_trap_frames();
    }
}

/// Non-consuming peek: returns `true` if a pending Java exception is set.
///
/// Used by `jit_invoke_dispatch` (and its bail/cache paths) to decide
/// whether to return the `i64::MIN` deopt sentinel — so the JIT caller's
/// post-invoke exception guard fires and the interpreter routes the
/// stashed exception through the method's exception table — instead of
/// returning a bogus `0` that the JIT would keep computing with.
/// Resolves the thread through [`current_jit_thread_ptr`] rather than taking a
/// `&mut JvmThread`, because three of its four call sites sit *before* the
/// `jit_thread_mut()` acquisition in their function and cannot name one. That
/// is sound here and only here: this is a **read**, so it creates no aliasing
/// `&mut`, and every call site runs inside a JIT helper on the thread that
/// installed the pointer. A null pointer (no JIT thread installed — the only
/// way to reach that is from outside JIT dispatch, where nothing can have
/// stashed an exception) reads as "nothing pending", which is the same answer
/// the empty TLS cell used to give.
pub fn jit_pending_exception_is_set() -> bool {
    let t = current_jit_thread_ptr();
    if t.is_null() {
        return false;
    }
    // SAFETY: `t` is this OS thread's own `JvmThread`, installed by
    // `set_jit_thread` and alive for the whole JIT call; the read is a
    // shared-reference-shaped `Option` load with no aliasing `&mut` created.
    unsafe { (*t).jit_pending_exception.is_some() }
}

/// Take (consume) a pending AIOOBE from JIT bounds check.
/// Returns `Some((index, length))` if an AIOOBE was pending.
pub fn take_jit_pending_aioobe() -> Option<(i64, i64)> {
    JIT_SIGNALS.with(|s| s.aioobe.take())
}

/// Take (consume) a pending `ArithmeticException` ("/ by zero") set by the JIT
/// integer-division zero-divisor guard. Returns `true` if one was pending.
/// Mirrors [`take_jit_pending_aioobe`]; the interpreter's post-JIT drain throws
/// a real `ArithmeticException` through the method's exception table.
pub fn take_jit_pending_arithmetic() -> bool {
    JIT_SIGNALS.with(|s| s.arithmetic.take())
}

/// Put a compiled-frame snapshot back into the signal record, without touching
/// any flag.
///
/// For a door that drained the snapshot to build a throwable it then had to
/// DISCARD: the flag is restored separately (`set_jit_pending_npe_flag_only`),
/// and the frames have to go with it or the next throwable is built from a bare
/// flag and the trace names only the frames the interpreter still holds.
///
/// Assigns the slot, so it must be called at most once per restash — the same
/// rule `handle_compiled_callee_deopt_sentinel`'s three-way restash states, for
/// the same reason: there is ONE `trap_frames` cell and a second call with
/// `None` wipes what the first put back.
pub fn restash_jit_pending_trap_frames(
    frames: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
) {
    JIT_SIGNALS.with(|s| *s.trap_frames.borrow_mut() = frames);
}

/// Re-stash a previously taken pending-arithmetic flag. Mirrors
/// [`stash_jit_pending_aioobe`] for the OSR drain-without-route path, so a
/// div-by-zero raised in OSR-compiled code with no in-frame handler survives the
/// OSR→interpreter handoff and is surfaced by the next JIT-return drain.
pub fn stash_jit_pending_arithmetic() {
    JIT_SIGNALS.with(|s| s.arithmetic.set(true));
}

/// Take (consume) a pending NPE from a JIT array helper (`jit_iaload`,
/// `jit_aaload`, `jit_arraylength`). Returns `true` if an NPE was pending.
///
/// The JVM specifies that all three opcodes throw `NullPointerException`
/// when their array reference is null; the JIT array helpers previously
/// swallowed the null silently (returning 0 / -1), which let JIT'd
/// Java code continue with corrupt state. Now they set this flag, return
/// `i64::MIN`, and the interpreter post-JIT path constructs the real
/// `java/lang/NullPointerException` and routes it through the method's
/// exception table — same pattern as `take_jit_pending_aioobe`.
pub fn take_jit_pending_npe() -> bool {
    JIT_SIGNALS.with(|s| s.npe.take())
}

/// Read the JEP-358 action code recorded alongside a pending JIT NPE WITHOUT
/// consuming it.
///
/// For a door that drained the flag with [`take_jit_pending_npe`] and may put
/// it back: it needs the code to build a message, and must not take it, because
/// the drain that eventually services the NPE reads the same cell.
pub fn peek_jit_pending_npe_action() -> u8 {
    JIT_SIGNALS.with(|s| s.npe_action.get())
}

/// Take (consume) the JEP-358 *action code* recorded alongside a pending JIT
/// NPE (`helpful_npe::jit_action::*`; `0` when none was recorded — e.g. the NPE
/// came from a helper that doesn't carry its operation kind). The interpreter
/// drain calls this right after [`take_jit_pending_npe`] to build the
/// action-only message.
pub fn take_jit_pending_npe_action() -> u8 {
    JIT_SIGNALS.with(|s| s.npe_action.take())
}

/// Internal: set the pending-NPE flag with no action code (the existing bare
/// signal — yields an unmessaged NPE). Resets the action cell to `0` so a stale
/// code from a prior op can never leak onto this NPE.
#[inline]
pub fn set_jit_pending_npe() {
    JIT_SIGNALS.with(|s| {
        s.npe.set(true);
        s.npe_action.set(0);
    });
    snapshot_trap_frames(0);
}

/// Internal: set the pending-NPE flag *with* a JEP-358 action code
/// (`helpful_npe::jit_action::*`). Called from the array/length helpers that
/// know their operation kind, so the interpreter can attach an action-only
/// JEP-358 message to the JIT-originated NPE.
#[inline]
pub fn set_jit_pending_npe_action(code: u8) {
    set_jit_pending_npe_action_at(code, 0);
}

/// As [`set_jit_pending_npe_action`], with the id of the inline null-check
/// site that trapped (`0` when the caller has none).
///
/// Only the inline null-check stubs can name a site: they are the shape whose
/// bci is otherwise unrecoverable, because the check is not a GC-capable call
/// and so publishes no safepoint id. Every other NPE-signalling helper is a
/// call, and its frame's slot already holds an id `activation_bci` can use.
#[inline]
pub fn set_jit_pending_npe_action_at(code: u8, trap_key: u32) {
    JIT_SIGNALS.with(|s| {
        s.npe.set(true);
        s.npe_action.set(code);
    });
    snapshot_trap_frames(trap_key);
}

/// The pending NPE of an `athrow` on a null reference in compiled code
/// (`jit_throw_exception`'s null arm; JIT round 20 wave 7, lane fuzzC,
/// correctness, unconditional; probe `R20FuzzCAthrowNull1`).
///
/// The arm used to raise the bare signal ([`set_jit_pending_npe`]: action
/// `NONE`, a snapshot placed by the frame's safepoint-id slot). An `athrow` is
/// not a published safepoint, so that slot holds the frame's LAST call's bci
/// (or the prologue's unset sentinel): the drain could corroborate nothing,
/// the NPE came out with `getMessage() == null` where HotSpot (and this VM's
/// interpreter) says `Cannot throw exception because "<expr>" is null`, and its
/// top frame carried the last call's line instead of the `athrow`'s.
///
/// `bci` is the `athrow`'s own bci, which both x64 tiers pass as a compile-time
/// immediate (the enclosing method's: neither builds an `athrow` inside a
/// spliced body). It goes on the innermost frame as the direct-throw helpers'
/// stamped bci does (`conservative_roots::apply_athrow_trap_bci`, which refuses
/// any frame it cannot vouch for, e.g. an aarch64 artifact, and then keeps the
/// walk's answer), and the [`ATHROW`] action code lets the drain read the
/// opcode there as the one that trapped only when it IS an `athrow`.
///
/// [`ATHROW`]: cratonvm_jit_api::npe_action::ATHROW
pub fn set_jit_pending_npe_athrow(bci: i64) {
    JIT_SIGNALS.with(|s| {
        s.npe.set(true);
        s.npe_action
            .set(crate::runtime::exceptions::helpful_npe::jit_action::ATHROW);
    });
    if !trap_frame_snapshot_enabled() {
        return;
    }
    let mut frames = trap_snapshot_walk();
    crate::jit::conservative_roots::apply_athrow_trap_bci(&mut frames, bci);
    JIT_SIGNALS.with(|s| {
        *s.trap_frames.borrow_mut() = (!frames.is_empty()).then_some(frames);
    });
}

/// Re-stash a previously-taken JIT NPE action code (OSR drain-without-route
/// path, mirroring [`stash_jit_pending_npe`]). Preserves the action so a
/// re-surfaced NPE keeps its JEP-358 message.
pub fn stash_jit_pending_npe_action(code: u8) {
    set_jit_pending_npe_action(code);
}

/// Set the out-of-band deopt/exception signal (MEDIUM fix: `i64::MIN` sentinel
/// collision). MUST be called on EVERY path that produces the `i64::MIN` deopt
/// sentinel, so the interpreter can tell a genuine deopt/exception apart from a
/// method that legitimately returns `Long.MIN_VALUE`. Idempotent; cheap.
///
/// Crate-public so the out-of-line JIT stubs in `jit/src/x64.rs` can route
/// through a tiny `extern "C"` helper that sets it (see [`jit_set_deopt_pending`]).
#[inline]
pub fn set_jit_deopt_pending() {
    JIT_SIGNALS.with(|s| {
        s.deopt.set(true);
        // Round 13 wave 8 (lane replay4): not a frameless trap stub unless
        // `jit_uncommon_trap` says so right after this.
        s.frameless_trap.set(FramelessTrapSite::Helper);
    });
    // P1 shadow record (`docs/threading/thread-transition-states.md` §7.2):
    // the deopt trap is the `CompiledUninterruptible -> Deoptimizing` edge —
    // the only tabled way into that state. From here until the interpreter has
    // materialised the frames, the thread holds `FrameValue` buffers that are
    // in neither the compiled frame's oop map nor a not-yet-built interpreter
    // frame, which is exactly what makes the window its own state.
    //
    // The window is closed by whichever comes first: the JIT entry pop
    // (`conservative_roots::leaving_compiled_state`, which resolves
    // `Deoptimizing` to `JavaRunning` rather than manufacturing an untabled
    // edge) or the interpreter's own resume sites.
    thread_state::record_transition(
        ThreadExecState::Deoptimizing,
        "jit::helpers::set_jit_deopt_pending",
    );
}

/// Read+clear the out-of-band deopt/exception signal. The interpreter's
/// post-invoke check calls this when it observes an `i64::MIN` return: `true`
/// means the JIT took the exception/deopt path and the `i64::MIN` is the
/// sentinel (route/re-run); `false` means `i64::MIN` is a real returned value
/// and must be pushed verbatim. See [`set_jit_deopt_pending`].
#[inline]
pub fn take_jit_deopt_pending() -> bool {
    JIT_SIGNALS.with(|s| s.deopt.take())
}

/// `extern "C"` trampoline for the out-of-line deopt/exception stubs emitted in
/// `jit/src/x64.rs`. Those stubs already `CALL` a helper (e.g. `jit_bastore`,
/// `jit_throw_aioobe`, the dispatch helper) that sets one of the pending-exception
/// flags AND `JIT_DEOPT_PENDING` before producing `i64::MIN`. This dedicated,
/// side-effect-free trampoline lets a stub that does NOT otherwise call such a
/// helper (or whose helper predates this flag) set the deopt signal with a
/// single `CALL` and no argument marshalling.
///
/// SAFETY: no pointer arguments; only touches a thread-local. Safe to call from
/// JIT-compiled code at any point before loading the `i64::MIN` sentinel.
pub extern "C" fn jit_set_deopt_pending() {
    set_jit_deopt_pending();
}

/// `i64::MIN`-sentinel disambiguation for `J`/`D` (long/double) call returns —
/// PEEK (non-clearing) of every out-of-band exception/deopt signal.
///
/// A compiled caller signals a callee exception/deopt by the dispatch helpers'
/// `i64::MIN` return in RAX. For `int`/ref/void returns that is unambiguous (no
/// such legitimate value), so the post-invoke check is a plain
/// `CMP RAX, i64::MIN; JE bail`. But a callee that *legitimately* returns
/// `Long.MIN_VALUE` (a `J`/`D` whose bits equal `i64::MIN`) returns the SAME
/// value with NO pending-signal flag set — so a `J`/`D` call site cannot tell the
/// two apart from RAX alone.
///
/// At a `J`/`D` call site the backend therefore emits, ONLY on the (rare)
/// `RAX == i64::MIN` branch, a `CALL` here. We peek (do not clear) every signal a
/// dispatch helper or its callee could have raised before returning `i64::MIN`:
///
///   * `JIT_PENDING_EXCEPTION`  — an explicit/native throwable (athrow, NPE-on
///     -receiver, re-stashed callee exception),
///   * `JIT_PENDING_NPE`        — a void-return-store / null-receiver NPE,
///   * `JIT_PENDING_AIOOBE`     — a bounds-check failure,
///   * `JIT_DEOPT_PENDING`      — the generic out-of-band deopt flag (set by
///     `jit_throw_aioobe` / `jit_uncommon_trap` / the x64 stubs), and
///   * a stashed IR-deopt frame (`cratonvm_jit::deopt::has_last_deopt`) — an
///     IR-path deopt of a dispatched callee returns `i64::MIN` WITHOUT the VM
///     flag (the stash lives in the jit crate).
///
/// Returns `1` iff ANY is pending (the `i64::MIN` is a genuine sentinel — the
/// caller bails through its epilogue, and the interpreter's post-JIT path drains
/// the still-set flag and routes/resumes), `0` iff none is pending (the
/// `i64::MIN` is a real `Long.MIN_VALUE` return — the caller keeps it). The peek
/// is non-destructive so the outer interpreter drain still observes the flag.
///
/// SAFETY: no pointer arguments; only reads this thread's own signal block and
/// `JvmThread`. Safe to call from JIT-compiled code immediately after a
/// dispatch returns `i64::MIN`.
pub extern "C" fn jit_dispatch_threw() -> i64 {
    // The throwable half lives on the `JvmThread` (GC-rooted); the scalar
    // flags stay in TLS. Both are this OS thread's own state, so the peek is
    // still allocation-free and lock-free — it just reads two places instead
    // of one. See `jit_pending_exception_is_set` for why resolving the thread
    // through the raw pointer is sound in a read-only peek.
    // `arithmetic` too (interpreter round i1 wave 4, L7): a `/ by zero` is the
    // one implicit signal this peek did not read. It is raised beside the deopt
    // flag, but a door that restores it flag-only after a nested door has
    // drained everything (`route_implicit_exc_through_callee`'s
    // `restash_and_return`) hands back a `J`/`D` sentinel with the arithmetic
    // flag alone, which read as a genuine `Long.MIN_VALUE` and lost the throw.
    let pending = (jit_pending_exception_is_set()
        || JIT_SIGNALS.with(|s| {
            s.npe.get() || s.aioobe.get().is_some() || s.arithmetic.get() || s.deopt.get()
        }))
        || cratonvm_jit::deopt::has_last_deopt();
    if pending {
        1
    } else {
        0
    }
}

/// JEP 358 (helpful NPE), inline-codegen path — `extern "C"` trampoline for the
/// per-action inline null-check failure stubs emitted in
/// `jit/src/x64.rs::emit_null_check_store_stubs`.
///
/// Each inline array load/store/`arraylength` null-check site is grouped by its
/// JEP-358 [`cratonvm_jit_api::npe_action`] code, and the per-action stub passes
/// that code here. We set the pending-NPE flag *with* the code (so the
/// interpreter's post-JIT NPE drain attaches the right action-only message —
/// "Cannot load from int array", …, gated behind
/// `-XX:+ShowCodeDetailsInExceptionMessages`) and the out-of-band deopt signal
/// (the stub loads `i64::MIN` as the method return value, so the interpreter
/// must not mistake it for a real `Long.MIN_VALUE`).
///
/// This replaces the old single shared stub, which called `jit_bastore(0)` and
/// therefore fabricated `ASTORE_BYTE` ("Cannot store to byte array") for *every*
/// inline array opcode regardless of its real element type or load/store
/// direction. `code == NONE` (0) sets a bare (unmessaged) NPE — the same shape
/// as today's default path — which is what the non-array intrinsic null-check
/// sites use.
///
/// SAFETY: no pointer arguments; only touches thread-locals. Safe to call from
/// JIT-compiled code at any point before loading the `i64::MIN` sentinel. The
/// `code` is truncated to `u8`; an out-of-range value maps to no message
/// (`jit_action_message` returns `None`), never a panic.
pub extern "C" fn jit_npe_with_action(code: i64) {
    let f = || jit_npe_with_action_body(code);
    contain("jit_npe_with_action", OnPanic::Deopt, (), f)
}

/// Body of [`jit_npe_with_action`], run under its panic guard.
pub fn jit_npe_with_action_body(code: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache,
    // matching every other array/field helper's entry (the next GC must
    // re-scan after we deopt back out to the interpreter).
    crate::jit::conservative_roots::note_jit_boundary();
    // The argument is PACKED: the low byte is the JEP-358 action code, and the
    // upper 24 bits are the inline null-check site id, or zero. A stub that
    // sets only the action (the historical shape, and the reason-10 precise
    // putfield path) therefore reads back as `trap_key == 0` with no special
    // case. See `x64::inlining::NpeTrapSite` and
    // `x64::Compiler::emit_null_check_store_stubs`.
    //
    // `as u64` before the shift, not `as u32`: the sign of an `i64` argument is
    // nobody's business here and an arithmetic shift on a hypothetical negative
    // would fabricate a key.
    let packed = code as u64;
    // Truncation: the low byte IS the action code by construction.
    let action = (packed & 0xff) as u8;
    // Truncation: `record_npe_trap_site` caps ids at 24 bits.
    let trap_key = ((packed >> 8) & 0x00ff_ffff) as u32;
    // nio21 wave 17 L5: a single-pass pad whose local-handler probe follows
    // stamped its bci (`jit_set_throw_bci`, the call just before this one)
    // with the defer bit. Strip it, and leave the frame snapshot to the probe.
    let (stamped, deferred) = cratonvm_jit::implicit_null::split_trap_snapshot_defer_bit(
        JIT_SIGNALS.with(|s| s.athrow_bci.get()),
    );
    if deferred {
        JIT_SIGNALS.with(|s| {
            s.athrow_bci.set(stamped);
            s.npe.set(true);
            s.npe_action.set(action);
        });
        defer_trap_snapshot(DeferredTrapSnapshot::NpeKey(trap_key));
    } else {
        set_jit_pending_npe_action_at(action, trap_key);
    }
    // Out-of-band deopt signal: the stub loads `i64::MIN` as the return value
    // (same invariant as `jit_bastore`'s null arm did before).
    set_jit_deopt_pending();
}

/// Obtain an exclusive reference to the JIT thread. Returns `None` if not set,
/// otherwise the `&mut JvmThread` paired with a [`JitThreadGuard`] RAII token.
///
/// The caller MUST keep the guard alive for as long as it uses the returned
/// reference (binding it to `_guard` is sufficient). When the guard drops it
/// clears the debug reentrancy flag; a nested/aliasing `jit_thread_mut` call
/// made while a prior guard is still live trips a `debug_assert!`.
///
/// # Safety
/// Caller must ensure this is only called from JIT helper functions on the same
/// thread that called `set_jit_thread`, and that no other reference to the
/// JvmThread is live.
// SAFETY: Caller must ensure this is only called from JIT helper functions on the
// same thread that called `set_jit_thread`, and that no other `&mut JvmThread`
// reference is live. The pointer was set by `set_jit_thread` from a valid `&mut JvmThread`.
// The `JIT_THREAD_BORROWED` flag + `JitThreadGuard` enforce the "no aliasing
// borrow" half of this invariant in debug builds; release builds are unaffected.
#[inline]
// SAFETY: invoked on the thread that installed `JIT_THREAD`, so no other
// `&mut JvmThread` is live; the returned exclusive borrow is unique for the
// guard's lifetime.
pub unsafe fn jit_thread_mut() -> Option<(&'static mut JvmThread, JitThreadGuard)> {
    let ptr = JIT_THREAD.with(|t| t.get());
    if ptr.is_null() {
        None
    } else {
        #[cfg(debug_assertions)]
        JIT_THREAD_BORROWED.with(|b| {
            // This now fires ONLY for a genuine *same-level* aliasing
            // fabrication: two live `jit_thread_mut` borrows that did NOT cross
            // a `set_jit_thread` re-entry boundary. The common case — an outer
            // `jit_invoke_dispatch` bailing into the interpreter, which
            // re-enters JIT and recurses into a second `jit_invoke_dispatch` —
            // is a *strictly nested* reborrow (the inner `&mut *ptr` descends
            // from the outer's `&mut` via the `set_jit_thread(thread)` cast),
            // and `set_jit_thread`/`restore_jit_thread` suspend+restore this
            // flag around that boundary so the legitimate nesting does NOT trip
            // here. (Empirically verified: DaCapo avrora drives ~1100 such
            // nested borrows and completes cleanly with no UB.)
            if b.get() {
                // Name BOTH sides before tripping. The assertion's own message
                // asserts this is "a genuine sibling fabrication"; that claim
                // is only checkable if the prior borrow's site is printed
                // next to this one's.
                let prior = JIT_THREAD_BORROW_SITE.with(|s| s.take());
                match prior {
                    Some(bt) => eprintln!(
                        "[jit-borrow] PRIOR borrow was taken here:\n{bt}\n\
                         [jit-borrow] SECOND borrow is being taken here:\n{}",
                        std::backtrace::Backtrace::force_capture()
                    ),
                    None => eprintln!(
                        "[jit-borrow] aliasing borrow detected; re-run with \
                         CRATONVM_DBG_JIT_BORROW_SITES=1 to see both sites"
                    ),
                }
            }
            debug_assert!(
                !b.get(),
                "jit_thread_mut: aliasing &mut JvmThread borrow detected \
                 (a prior JitThreadGuard is still live at the SAME JIT nesting \
                 level — this is a genuine sibling fabrication, not a re-entry)"
            );
            b.set(true);
            if jit_borrow_site_capture_enabled() {
                JIT_THREAD_BORROW_SITE
                    .with(|s| s.set(Some(Box::new(std::backtrace::Backtrace::force_capture()))));
            }
        });
        Some((
            &mut *ptr,
            JitThreadGuard {
                #[cfg(debug_assertions)]
                _private: (),
            },
        ))
    }
}

/// Attempt to call a JIT-compiled entry through the transmute tables.
/// Returns `Some(rc)` on success, `None` when the arg count exceeds the
/// table coverage (16 ABI words since round 12 wave 2, context included --
/// `try_call_compiled_entry_wide`; 8 under its kill switch: the tables were
/// widened past the register file, and the platform C ABI puts the overflow
/// on the stack exactly where a compiled prologue reads it). The
/// caller is expected to fall through to the interpreter slow-path in
/// `jit_invoke_dispatch` (which decodes args into `Value`s and bails)
/// rather than returning the previous silent `0` — that was the round-5
/// MED follow-up to the round-4 wave-2 fix for the helper dispatch path.
///
/// CRIT (round-5 review): three sibling sites in `jit_invoke_dispatch`
/// (the thread-local DISPATCH_CACHE fast-path, the JIT cache fast-path,
/// and the post-compile fast-path) each carried `_ => 0` arms that
/// silently dropped 5+-arg callees. Sharing this helper keeps the
/// register-table dispatch in one place and removes those drop sites.
///
/// SAFETY: `entry` must be a live JIT-compiled extern "C" entry point
/// whose calling convention matches `needs_ctx` (with-ctx prepends an
/// `i64` VM pointer to the Java arg slots). `args_slice` must contain
/// exactly `args_slice.len()` valid i64 arg slots; on overflow we don't
/// dereference the table at all.
#[inline]
// SAFETY: `entry` is a live JIT-compiled `extern "C"` code pointer produced by
// the compiler; it is transmuted to a fn signature matching the (with/without
// NativeContext) arity actually called below.
pub unsafe fn try_call_compiled_entry(
    entry: usize,
    needs_ctx: bool,
    vm_ptr: i64,
    args_slice: &[i64],
) -> Option<i64> {
    // The compiled callee's prologue publishes ITS rbp into the precise-maps
    // innermost-RBP mirror; nothing on this Rust path republishes the JIT
    // caller's rbp after the callee returns, so a later GC would walk the
    // dead callee frame (see `top_rbp_mirror_write`). Snapshot + restore the
    // mirror around the raw entry call.
    //
    // Both halves or neither: the callee's prologue publishes its IDENTITY
    // beside its rbp, so restoring only the rbp would leave the mirrors naming
    // two different frames — the caller's rbp with the callee's identity — and
    // nothing downstream can detect that, because both halves would still read
    // consistently out of the mirrors. See `top_cm_id_mirror_read`.
    let saved_top_rbp = crate::jit::conservative_roots::top_rbp_mirror_read();
    let saved_cm_id = crate::jit::conservative_roots::top_cm_id_mirror_read();
    let r = try_call_compiled_entry_inner(entry, needs_ctx, vm_ptr, args_slice);
    crate::jit::conservative_roots::top_rbp_mirror_write(saved_top_rbp);
    crate::jit::conservative_roots::top_cm_id_mirror_write(saved_cm_id);
    r
}

// SAFETY: same entry ABI contract as `try_call_compiled_entry`.
pub unsafe fn try_call_compiled_entry_inner(
    entry: usize,
    needs_ctx: bool,
    vm_ptr: i64,
    args_slice: &[i64],
) -> Option<i64> {
    let n = args_slice.len();
    // Past the eight-word tables below: see `try_call_compiled_entry_wide`.
    if n + usize::from(needs_ctx) > HELPER_ENTRY_TABLE_WORDS {
        return try_call_compiled_entry_wide(entry, needs_ctx, vm_ptr, args_slice);
    }
    if needs_ctx {
        Some(match n {
            0 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry (caller contract);
                // the 0-arg with-ctx callee takes exactly (vm_ptr) per the JIT ABI.
                let f: unsafe extern "C" fn(i64) -> i64 = std::mem::transmute(entry);
                f(vm_ptr)
            }
            1 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry; the 1-arg with-ctx
                // callee takes (vm_ptr, arg0) per the JIT ABI.
                let f: unsafe extern "C" fn(i64, i64) -> i64 = std::mem::transmute(entry);
                f(vm_ptr, args_slice[0])
            }
            2 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry; the 2-arg with-ctx
                // callee takes (vm_ptr, arg0, arg1) per the JIT ABI.
                let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(entry);
                f(vm_ptr, args_slice[0], args_slice[1])
            }
            3 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry; the 3-arg with-ctx
                // callee takes (vm_ptr, arg0, arg1, arg2) per the JIT ABI.
                let f: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 = std::mem::transmute(entry);
                f(vm_ptr, args_slice[0], args_slice[1], args_slice[2])
            }
            4 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    vm_ptr,
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                )
            }
            5 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    vm_ptr,
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                    args_slice[4],
                )
            }
            6 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    vm_ptr,
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                    args_slice[4],
                    args_slice[5],
                )
            }
            7 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    vm_ptr,
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                    args_slice[4],
                    args_slice[5],
                    args_slice[6],
                )
            }
            _ => return None,
        })
    } else {
        Some(match n {
            0 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry (caller contract);
                // the 0-arg no-ctx callee takes no arguments per the JIT ABI.
                let f: unsafe extern "C" fn() -> i64 = std::mem::transmute(entry);
                f()
            }
            1 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry; the 1-arg no-ctx
                // callee takes (arg0) per the JIT ABI.
                let f: unsafe extern "C" fn(i64) -> i64 = std::mem::transmute(entry);
                f(args_slice[0])
            }
            2 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry; the 2-arg no-ctx
                // callee takes (arg0, arg1) per the JIT ABI.
                let f: unsafe extern "C" fn(i64, i64) -> i64 = std::mem::transmute(entry);
                f(args_slice[0], args_slice[1])
            }
            3 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry; the 3-arg no-ctx
                // callee takes (arg0, arg1, arg2) per the JIT ABI.
                let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(entry);
                f(args_slice[0], args_slice[1], args_slice[2])
            }
            4 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry; the 4-arg no-ctx
                // callee takes (arg0, arg1, arg2, arg3) per the JIT ABI.
                let f: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 = std::mem::transmute(entry);
                f(args_slice[0], args_slice[1], args_slice[2], args_slice[3])
            }
            5 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                    args_slice[4],
                )
            }
            6 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                    args_slice[4],
                    args_slice[5],
                )
            }
            7 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                    args_slice[4],
                    args_slice[5],
                    args_slice[6],
                )
            }
            8 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                    args_slice[4],
                    args_slice[5],
                    args_slice[6],
                    args_slice[7],
                )
            }
            _ => return None,
        })
    }
}

/// ABI words (the context pointer included) the per-arity arms of
/// [`try_call_compiled_entry_inner`] cover: 8 with no context, 7 Java
/// arguments plus the context.
pub const HELPER_ENTRY_TABLE_WORDS: usize = 8;

/// ABI words [`try_call_compiled_entry_wide`] passes: every compiled-entry
/// call from a helper whose context pointer plus Java arguments fit in it.
pub const HELPER_ENTRY_MAX_WORDS: usize = 16;

/// `CRATONVM_JIT_HELPER_WIDE_ENTRY_CALLS` (default on; `0` restores the
/// eight-word cap, so a wider compiled callee reached through a helper runs
/// interpreted as before round 12 wave 2). Cached once per process: it only
/// selects between two correct routes.
pub fn helper_wide_entry_calls_enabled() -> bool {
    pub static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        true
    })
}

/// `CRATONVM_JIT_DISPATCH_VIRTUAL_MEMO_FIRST` (default on; `0` restores the
/// order before round 12 wave 2): `jit_invoke_dispatch`'s virtual arm consults
/// its `(site, receiver class)` compiled-entry memo before resolving the
/// dispatch target, instead of after. Cached once per process.
pub fn dispatch_virtual_memo_first_enabled() -> bool {
    pub static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        true
    })
}

/// A compiled-entry call wider than the per-arity tables (round 12 wave 2,
/// lane `calls`): 9..=[`HELPER_ENTRY_MAX_WORDS`] ABI words.
///
/// Before this, every such call answered `None`, and each caller's fallback
/// ended in the interpreter: the MIC-hit arm bailed by name, the megamorphic
/// arm fell through to the resolving arm (a compile probe per call) and then
/// `invoke_or_native`, whose door refuses more than eight words too
/// (`jit_bridge::jit_abi_max_java_params`), so the compiled callee's
/// BYTECODE ran. On Win64 that is any instance callee with seven or more
/// arguments that takes the context (four ABI registers), which is how
/// `R12CallsMicMissCompiled` (receiver + 7) measured 19x HotSpot.
///
/// The call pads the arguments with zeros to one fixed sixteen-word
/// signature. Both x86-64 C ABIs are caller-cleanup and pass the words past
/// the register file on the caller's stack in order (Win64 above the 32-byte
/// home area, SysV at the top), which is exactly where every compiled prologue
/// reads its stack arguments (`x64::frames` and `ir_lower`'s
/// `STACK_ARG_BASE`); a callee reads only the words it declares, so the
/// padding is never observed. The per-arity arms above already rely on the
/// same layout for their stack words.
#[inline(never)]
// SAFETY: same entry ABI contract as `try_call_compiled_entry`.
pub unsafe fn try_call_compiled_entry_wide(
    entry: usize,
    needs_ctx: bool,
    vm_ptr: i64,
    args_slice: &[i64],
) -> Option<i64> {
    let lead = usize::from(needs_ctx);
    // x86-64 only: the layout argument above is the two x86-64 C ABIs' and
    // the x86-64 prologues'; another backend keeps the interpreter fallback.
    if !cfg!(target_arch = "x86_64")
        || args_slice.len() + lead > HELPER_ENTRY_MAX_WORDS
        || !helper_wide_entry_calls_enabled()
    {
        return None;
    }
    let mut w = [0i64; HELPER_ENTRY_MAX_WORDS];
    if needs_ctx {
        w[0] = vm_ptr;
    }
    for (slot, &arg) in w[lead..].iter_mut().zip(args_slice) {
        *slot = arg;
    }
    type Wide = unsafe extern "C" fn(
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    ) -> i64;
    // SAFETY: `entry` is a live compiled entry (caller contract) whose
    // prologue reads `args_slice.len() + lead` words in the C ABI's layout;
    // the trailing zero words sit past them and are never read.
    let f: Wide = std::mem::transmute(entry);
    Some(f(
        w[0], w[1], w[2], w[3], w[4], w[5], w[6], w[7], w[8], w[9], w[10], w[11], w[12], w[13],
        w[14], w[15],
    ))
}

/// Call a compiled entry from a JIT dispatch helper while suspending the
/// debug borrow tracker for the outer JIT frame.
///
/// A compiled callee can immediately re-enter JIT dispatch and borrow the
/// same `JvmThread`. That nested borrow is a child reborrow of the outer JIT
/// frame, but the direct fast paths do not go through `set_jit_thread`, so the
/// debug tracker must be suspended explicitly around the raw call.
#[inline]
// SAFETY: same entry ABI contract as `try_call_compiled_entry`.
pub unsafe fn try_call_compiled_entry_reentrant(
    entry: usize,
    needs_ctx: bool,
    vm_ptr: i64,
    args_slice: &[i64],
) -> Option<i64> {
    note_cached_entry_arm(&CACHED_ENTRY_REGISTRY_PINS);
    // This helper is itself called from compiled dispatch code.  Its raw ABI
    // call used to enter the nested compiled method without registering a
    // `JitEntryGuard`, so a GC triggered by that callee found a JIT return
    // address above an empty guard chain.  The moving-young collector then
    // had to fall back to conservative/non-moving collection and retained
    // the repeated Hibernate bootstrap graphs until OOM.  Resolve the entry
    // back to its live CompiledMethod and register the precise frame for the
    // full duration of the nested call.
    // Pin, don't peek. This is the ONE path into compiled code that used to hold
    // no owning reference to the body it entered: it resolved a bare `cm_ptr`
    // out of the code-range registry and dereferenced it, on the argument that
    // "the registry owns this CompiledMethod while its entry remains callable".
    // The registry stores a raw address, not an `Arc`, so it owns nothing — and
    // the caller's keep-alive here is a *thread-local dispatch cache entry*
    // (`try_mic_rust_cached_entry` reads `entry`/`needs_context` out of the map
    // and calls the raw pointer). A nested dispatch from the callee re-enters
    // `flush_raw_entry_dispatch_caches`, whose `clear()` drops exactly that
    // entry — measured releasing a published body at `active_jit_executions`
    // = 1 on `BasicErrorControllerIntegrationTests`.
    //
    // Holding the pin across the call is what makes the process-wide invariant
    // true: **a thread inside a compiled body always holds an owning reference
    // to it**, so a reference count reaching zero is itself a proof that no
    // thread is inside. One atomic increment (the registry carries a `Weak`).
    let pinned = match cratonvm_jit::pin_jit_code_range_owner_checked(entry) {
        cratonvm_jit::JitEntryPin::Live(owner) => Some(owner),
        cratonvm_jit::JitEntryPin::NotJit => None,
        // Round 14 wave 1 (lane codecache,
        // `r14w1-codecache-unpinned-raw-entry-call-patch-FIXED-20260929.md`): compiled
        // code whose owner is gone is never entered. `None` sends every caller
        // to its interpreter bail, as a call too wide for the tables does.
        // `CRATONVM_JIT_REFUSE_DEAD_ENTRY=0` calls it anyway (read only here).
        cratonvm_jit::JitEntryPin::Dead => return None,
    };
    call_compiled_entry_under_owner(pinned.as_deref(), entry, needs_ctx, vm_ptr, args_slice)
}

/// [`try_call_compiled_entry_reentrant`] for a caller that ALREADY holds an
/// owning reference to the callee's artifact.
///
/// The registry round trip the other form performs — an `ArcSwap` load, a binary
/// search over every registered code range, a `Weak::upgrade` CAS and the
/// matching `Arc` drop — re-derives an `Arc<CompiledMethod>` the thread-local
/// dispatch caches already hold in their [`cratonvm_jit::RetainedCode`] field.
/// A `RetainedCode` clone is one relaxed increment and answers the same
/// question, so the documented invariant is unchanged: **a thread inside a
/// compiled body always holds an owning reference to it**, which is what
/// `RetainedCode`'s own `Drop` relies on when it declines to route a
/// provably-not-last release through the retirement queue.
///
/// It is a clone and not a borrow because the map cannot be kept borrowed across
/// the call: a nested dispatch from the callee re-enters
/// `flush_raw_entry_dispatch_caches`, which takes `borrow_mut` on the same
/// thread-local — and, more to the point, may evict this very entry. The clone
/// is what survives that.
///
/// `CRATONVM_JIT='-cached-entry-owner-reuse'` sends these callers back through
/// the registry so one binary can be A/B'd against itself, and the two counters
/// this bumps say which arm ran.
#[inline]
// SAFETY: same entry ABI contract as `try_call_compiled_entry_reentrant`.
pub unsafe fn try_call_compiled_entry_reentrant_owned(
    owner: &cratonvm_jit::RetainedCode,
    entry: usize,
    needs_ctx: bool,
    vm_ptr: i64,
    args_slice: &[i64],
) -> Option<i64> {
    if !cached_entry_owner_reuse_enabled() {
        return try_call_compiled_entry_reentrant(entry, needs_ctx, vm_ptr, args_slice);
    }
    note_cached_entry_arm(&CACHED_ENTRY_OWNER_REUSE_HITS);
    call_compiled_entry_under_owner(Some(&**owner), entry, needs_ctx, vm_ptr, args_slice)
}

/// `CRATONVM_JIT='-cached-entry-owner-reuse'` — make every cached compiled
/// dispatch re-resolve its keep-alive through the code-range registry, as it did
/// before 2026-08-17. Default ON.
pub fn cached_entry_owner_reuse_enabled() -> bool {
    pub static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// How often a site whose callee the compiler declined re-asks. See
/// [`MIC_COMPILE_DECLINED`].
pub const MIC_COMPILE_PROBE_RETRY: u32 = 1024;

/// Compile probes skipped because [`MIC_COMPILE_DECLINED`] already held a
/// refusal for this `(site, receiver class)`.
///
/// Counted separately from `not_probed_*`/`probe_returned_none` deliberately:
/// the comment on those says why a skip that lands in a "the compiler refused"
/// counter makes the number unreadable.
pub static MIC_COMPILE_PROBE_MEMO_SKIPS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Cached dispatches that reused the owner their cache entry already held.
pub static CACHED_ENTRY_OWNER_REUSE_HITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Cached dispatches that resolved the owner through the code-range registry.
pub static CACHED_ENTRY_REGISTRY_PINS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Count one arm of the cached-dispatch split, **behind the same gate as every
/// other counter in this file**.
///
/// The gate is not tidiness. The reuse arm runs 111 564 628 times in a 55 s
/// netty run, and an ungated `fetch_add` there is one contended cache line
/// shared by every thread in the VM — an instrument that would have been a
/// variable of the very comparison it exists to settle, present on one arm of
/// the A/B and absent from the other.
#[inline]
pub fn note_cached_entry_arm(counter: &'static std::sync::atomic::AtomicU64) {
    if mic_prof::enabled() {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The body both forms share: register the precise frame for the nested call,
/// correct a lying ABI flag against the artifact's own, and make the raw call.
#[inline]
// SAFETY: `entry` must be a live JIT entry owned by `compiled` when it is
// `Some`; `args_slice` must match the callee's ABI.
pub unsafe fn call_compiled_entry_under_owner(
    compiled: Option<&cratonvm_jit::CompiledMethod>,
    entry: usize,
    needs_ctx: bool,
    vm_ptr: i64,
    args_slice: &[i64],
) -> Option<i64> {
    let mut needs_ctx = needs_ctx;
    // An `if let` in THIS function's body, not `compiled.map(|..| ..)`: the
    // guard records `current_stack_pointer()` as the band's `entry_sp` /
    // `frame_base`, and inside a closure that is the sp of a frame that has
    // already RETURNED by the time `try_call_compiled_entry` pushes the callee.
    // Whenever the closure is not inlined, the callee's frame can then sit
    // ABOVE the recorded base, failing the scanners' `exact_rbp < frame_base`
    // test, and the fallback band `[scanner_sp, entry_sp)` does not cover it
    // (round 11, lane `rt`; the `JitEntryGuard` doc warns against exactly
    // this). `enter_with_compiled` and `enter_with_compiled_at` are both
    // `inline(always)`, so the sp below is this frame's own, as at the
    // method-entry door.
    let jit_root_guard = if let Some(compiled) = compiled {
        // cceres2 (WildFly SIGSEGV cores SF2/SF3/SM): the caller-supplied ABI
        // flag can come from a cache whose (entry, needs_context) pair was
        // read non-atomically across a concurrent inline-cache retarget or
        // C1->C2 supersede. Marshalling with the wrong flag shifts every
        // argument by one inside the callee (its `this` reads leftover
        // register junk — the receiver=0x2d0-style PIC-dispatch SIGSEGV).
        // The resolved CompiledMethod is the entry's OWN record; its flag is
        // definitive. Override and count/log the mismatch so the lying cache
        // can be identified.
        let own = compiled.needs_context();
        if own != needs_ctx {
            pub static MISMATCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = MISMATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 16 || n.is_power_of_two() {
                tracing::warn!(
                    "JIT ABI flag mismatch #{n}: cached needs_ctx={} but compiled entry 0x{:x} \
                     says {} — using the compiled method's own flag",
                    needs_ctx,
                    entry,
                    own,
                );
            }
            needs_ctx = own;
        }
        Some(crate::jit::conservative_roots::JitEntryGuard::enter_with_compiled(compiled))
    } else {
        None
    };
    // Entry hygiene, as `run_jit_body` and `try_osr` apply it: a compiled caller
    // that is issuing a call is not in an exceptional state, so a deopt flag
    // pending now cannot describe the callee about to run. Retiring it makes
    // the flag PER-CALL FRESH, which is what lets
    // `route_implicit_exc_through_callee` read "sentinel + flag + nothing
    // stashed" as this callee's own frameless trap and service it, instead of
    // mistaking a stale flag beside a genuine `Long.MIN_VALUE` for one. One
    // thread-local store.
    let _ = take_jit_deopt_pending();
    #[cfg(debug_assertions)]
    let borrow = suspend_jit_borrow();
    let result = try_call_compiled_entry(entry, needs_ctx, vm_ptr, args_slice);
    #[cfg(debug_assertions)]
    restore_jit_borrow(borrow);
    drop(jit_root_guard);
    // AFTER the guard: the caller's owning reference is what keeps the body
    // mapped for the whole call, so it must outlive both the call and the chain
    // entry naming it. Both callers hold theirs across this return.
    result
}

/// The constant-pool owner of a bridged `invokespecial` (`bail_to_interpreter`,
/// `--real-jdk`) through `caller`'s loader. For an isolated URL loader's
/// caller (`constants::class_entry_record_applies`) through the caller's
/// `CONSTANT_Class` entry naming `name` (its record first, a fresh resolution
/// recorded): `resolve_class_loader_aware` ignores such a loader's memo, so
/// every bridged call asked its `loadClass` again, where HotSpot asks once per
/// entry. `JitInvokeInfo` carries no constant-pool index, so the entry is the
/// caller's first `CONSTANT_Class` naming `name` (a pool scan, on this slow
/// path and for such a caller only); a substituted name the pool does not
/// hold resolves as before. Every other caller resolves exactly as before.
/// Round 15 wave 5, lane classsite2
/// (`r15w5-classsite-class-entry-shared-by-every-instruction-patch-FIXED-20260930.md`,
/// part D).
///
/// JIT round 16, lane classsite (proposal CS2-2): the other bridged owners
/// that re-resolved `JitInvokeInfo::class_name` by name -- the dispatch
/// tail's `invokespecial`, its cached-bytecode-callee door and the
/// `invokestatic` owner override -- come through here too, as
/// [`jit_bridged_owner_entry`], behind `CRATONVM_JIT_CLASS_ENTRY_DOORS`.
/// The `JitInvokeInfo` field the proposal named was not added: the struct
/// has ~180 literals across the jit and vm crates, and the pool search is
/// paid on a slow path by an isolated loader's caller only.
pub fn jit_bridged_special_owner(
    vm: &SharedVm,
    thread: &mut JvmThread,
    caller: ClassId,
    name: &str,
) -> Option<ClassId> {
    jit_bridged_owner(vm, thread, caller, name, false)
}

/// [`jit_bridged_special_owner`] for the bridged owners round 16 routed
/// through the entry (`CRATONVM_JIT_CLASS_ENTRY_DOORS`, default on; off,
/// they resolve by name as before).
pub fn jit_bridged_owner_entry(
    vm: &SharedVm,
    thread: &mut JvmThread,
    caller: ClassId,
    name: &str,
) -> Option<ClassId> {
    jit_bridged_owner(vm, thread, caller, name, true)
}

/// The body of [`jit_bridged_special_owner`] / [`jit_bridged_owner_entry`];
/// `switched` asks the round-16 switch past the (cheaper) record predicate.
pub fn jit_bridged_owner(
    vm: &SharedVm,
    thread: &mut JvmThread,
    caller: ClassId,
    name: &str,
    switched: bool,
) -> Option<ClassId> {
    if crate::runtime::interpreter::constants::class_entry_record_applies(vm, thread, caller)
        && (!switched || crate::runtime::interpreter::jit_bridge::jit_class_entry_doors_enabled())
    {
        // Before the pool read: the record is filed only while no
        // redefinition of `caller` may have replaced that pool.
        let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
        let entry = {
            let cm = vm.classes.class_manager.read();
            cm.get_class(caller).and_then(|class| {
                let pool = &class.constant_pool;
                let len = u16::try_from(pool.len()).unwrap_or(u16::MAX);
                (1..len).find(|&index| pool.get_class_name(index) == Some(name))
            })
        };
        if let Some(cp_index) = entry {
            return crate::runtime::interpreter::constants::resolve_class_entry_recorded(
                vm,
                thread,
                (caller, cp_index),
                name,
                fill_as_of,
            )
            .ok();
        }
    }
    crate::runtime::interpreter::resolve_class_loader_aware(vm, thread, caller, name).ok()
}

/// Bail a JIT-dispatched call out to the interpreter when the compiled
/// callee has more arguments than `call_jit_compiled_method_entry`'s
/// register-arg dispatch tables can pass. Issues `invoke_or_native` with
/// the full `Value` argument vector and converts the result back to the
/// `i64` register-ABI return value expected by the JIT caller. Exceptions
/// are stashed via `handle_jit_dispatch_error` so the interpreter post-JIT
/// path can route them through the caller's exception table.
#[inline(never)]
// SAFETY: called from a JIT helper with a live `SharedVm` and the current
// `JvmThread` set up, so resuming interpretation / routing through the caller's
// exception table operates on valid VM state.
pub unsafe fn bail_to_interpreter(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    args: &[Value],
) -> i64 {
    // This bail resolves the callee from a class NAME, and a lambda-proxy
    // receiver has no name of its own: it resolves as its functional interface,
    // which for four of them runs a collector native in place of the lambda
    // body. Same hazard as the dispatch slow path — see
    // `try_lambda_proxy_sam_dispatch`, which declines everything that is not a
    // proxy's SAM, so the routing below is unchanged for every other call.
    crate::vm::dbg_dispatch_tally(
        "jit_bail_to_interpreter",
        info.class_name,
        info.method_name,
        info.descriptor,
    );
    if let Some(result) = try_lambda_proxy_sam_dispatch(vm, thread, info, args) {
        return match result {
            Ok(Some(Value::Int(v))) => v as i64,
            Ok(Some(Value::Long(v))) => v,
            Ok(Some(Value::Float(f))) => f.to_bits() as i64,
            Ok(Some(Value::Double(d))) => d.to_bits() as i64,
            Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
            Ok(Some(Value::Object(None)) | None) => 0,
            Ok(_) => 0,
            Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
        };
    }
    // invokespecial (kind=1) must NOT virtually re-target onto the receiver's
    // runtime class — same rationale as the kind=1 arm of the dispatch slow
    // path below. `invoke_or_native` → `invoke_on_class_shared` applies the
    // abstract→receiver retarget, which turns a compiled super-call bridge
    // into a self-call loop: H2's `ValueVarchar.compareTypeSafe` is
    // `invokespecial ValueStringBase.compareTypeSafe` (4 args + ctx, so
    // `try_call_compiled_entry`'s register table always bails it here);
    // retargeting the abstract `ValueStringBase` back to the `ValueVarchar`
    // receiver re-enters the bridge → dispatch → bail → ∞, surfacing as the
    // GROUP BY `StackOverflowError` from the JIT depth guard (every H2
    // TreeMap<ValueRow> comparator walk dies). `invoke_special_shared`
    // preserves super-call semantics with native-override priority.
    //
    // JIT round 21 wave 14, lane hidden-static: a hidden caller's call of its
    // own method is dispatched on the caller by id, in either mode (the
    // by-name doors would load the class path's copy of the name).
    let hidden_self = if matches!(info.invoke_kind, 1 | 3) {
        jit_hidden_self_owner(vm, ClassId::new(info.declaring_class_id), info.class_name)
    } else {
        None
    };
    let res = if info.invoke_kind == 1 {
        // A substituted owner by its recorded id, as the dispatch arm does.
        // Otherwise, under `--real-jdk`, the constant-pool class through the
        // CALLER's loader, as the dispatch arm's tail does
        // (BUG-JIT-INVOKESPECIAL-LOADER-20260726): the by-name
        // `invoke_special_shared` binds whichever copy the global map holds.
        // `--compatible` keeps the by-name route here (interpreter round i1
        // wave 38, lane L6, review item 4).
        let mut owner = jit_substituted_owner(vm, info).or(hidden_self);
        if owner.is_none() && info.declaring_class_id != 0 && vm.config.is_jdk_only() {
            owner = jit_bridged_special_owner(
                vm,
                thread,
                ClassId::new(info.declaring_class_id),
                info.class_name,
            );
        }
        match owner {
            Some(owner) => crate::vm::invoke_special_shared_on_class(
                vm,
                thread,
                owner,
                info.class_name,
                info.method_name,
                info.descriptor,
                args,
            ),
            None => crate::vm::invoke_special_shared(
                vm,
                thread,
                info.class_name,
                info.method_name,
                info.descriptor,
                args,
            ),
        }
    } else if matches!(info.invoke_kind, 0 | 2) {
        // VIRTUAL DISPATCH FIX (bug #2 — regex zero-width corruption under
        // `CRATONVM_JIT_VIRTUAL_TIERUP`). For virtual/interface kinds,
        // `info.class_name` is the *static* call-site type, not the receiver's
        // runtime class. `invoke_or_native` resolves the callee against the
        // class name it is handed (it does NOT re-dispatch on `args[0]`), so a
        // static type that declares a *concrete* base method shadows the
        // receiver's override. The regex engine is the canonical victim: the
        // `root.match(...)` call site is statically typed `Pattern$Node`, whose
        // base `match` is an unconditional zero-width "accept" (`matcher.last =
        // i; return true`). Bailing on the static type ran that accept node at
        // every position instead of `Pattern$Start.match`, so `find()` reported
        // a zero-width match before every character and
        // `"org.x".replaceAll("[.]","/")` produced "/o/r/g/...". The compiled-
        // entry MIC path already resolves on the receiver class (see
        // `jit_invoke_virtual_mic`); this register-overflow / exception-reroute
        // bail must do the same. Mirrors the array→Object and synthetic-id
        // fallbacks used there.
        let dispatch_class = virtual_dispatch_class(vm, args, info);
        crate::vm::invoke_or_native(
            vm,
            thread,
            &dispatch_class,
            info.method_name,
            info.descriptor,
            args,
        )
    } else if let (3, Some(owner)) = (info.invoke_kind, hidden_self) {
        // A hidden caller's call of its own static method (see `hidden_self`),
        // on the caller by id (5.4.3.3 lookup and 5.5 initialization of the
        // declaring class), as the not-entrant static arm and the dispatch
        // tail's owner override do.
        crate::vm::invoke_static_shared_on_class(
            vm,
            thread,
            owner,
            info.class_name,
            info.method_name,
            info.descriptor,
            args,
        )
    } else if let Some(owner) = jit_hidden_static_owner(vm, info) {
        // JIT round 21 lane reflect4 (correctness, unconditional): the
        // hidden-owner leaf's class is found by its id only; by name this
        // re-run would look for a spun `LambdaForm$MH` no loader answers.
        crate::vm::invoke_static_shared_on_class(
            vm,
            thread,
            owner,
            info.class_name,
            info.method_name,
            info.descriptor,
            args,
        )
    } else if info.invoke_kind == 3 {
        // The static entry: JVMS §5.5 before a registered static native where
        // `static_by_name_init_enforced` holds; `--compatible` is censused
        // under door `jit_bail_static` until it is flipped.
        crate::vm::invoke_static_or_native(
            vm,
            thread,
            info.class_name,
            info.method_name,
            info.descriptor,
            args,
            "jit_bail_static",
        )
    } else {
        crate::vm::invoke_or_native(
            vm,
            thread,
            info.class_name,
            info.method_name,
            info.descriptor,
            args,
        )
    };
    match res {
        Ok(Some(Value::Int(v))) => v as i64,
        Ok(Some(Value::Long(v))) => v,
        Ok(Some(Value::Float(f))) => f.to_bits() as i64,
        Ok(Some(Value::Double(d))) => d.to_bits() as i64,
        Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
        Ok(Some(Value::Object(None))) | Ok(None) => 0,
        Ok(_) => 0,
        Err(e) => handle_jit_dispatch_error(vm, thread, e, info),
    }
}

/// Dispatch a SAM call on a lambda-proxy receiver through the proxy registry.
/// `None` means "not that" — not a virtual/interface call, not a lambda-proxy
/// receiver, or not the proxy's SAM — and the caller proceeds unchanged.
///
/// Every JIT path that resolves a callee BY NAME needs this in front of it. A
/// lambda proxy's class id is synthetic and absent from the class store, so
/// name resolution falls back to the constant-pool class — the FUNCTIONAL
/// INTERFACE — and CratonVM registers natives on four of those
/// (`java/util/function/Supplier.get`, `BiConsumer.accept`, `Function.apply`,
/// `BinaryOperator.apply`) to serve the synthetic objects
/// `Collector.accumulator()` and its siblings hand out. Those natives cannot
/// decline: with no collector tag in field 0 they take their catch-all arm, so
/// an ordinary user lambda routed to them has its body skipped entirely —
/// `Supplier.get` returns an empty `ArrayList`, `Function.apply` returns its
/// argument, `BiConsumer.accept(a, b)` calls `a.add(b)`, and
/// `BinaryOperator.apply(a, b)` calls `a.addAll(b)`. Three of those four are
/// SILENT.
///
/// The interpreter never had the problem — `try_lambda_dispatch` runs before
/// native resolution there — so the failure is JIT-only, and it additionally
/// needs the call site to be megamorphic, since a site that only ever sees one
/// proxy class stays on a cached path. That combination is why it took
/// Hibernate's `forEachSubInitializer(BiConsumer, ..)` — invoked with
/// `Initializer::startLoading`, `::resolveKey` and `::initializeInstance` — to
/// surface it, as `NoSuchMethodError: <sub-initializer>.add(Ljava/lang/Object;)Z`,
/// three failures in every full-class run of `ASTParserLoadingTest`.
/// `apps/hib-suite-runner/FunctionalInterfaceHijackProbe.java` is the reduced
/// witness for all four interfaces; `hql-ordinal-parameter-dropped-under-jit-20260731-FIXED.md` is the writeup.
///
/// Kept as one helper rather than repeated at each bail so a third by-name
/// dispatch path added later inherits the guard instead of re-opening the hole.
///
/// SAFETY: same contract as the callers — a live `SharedVm` and a current
/// `JvmThread`, since the SAM body may resume interpretation.
pub fn try_lambda_proxy_sam_dispatch(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    values: &[Value],
) -> Option<Result<Option<Value>, crate::error::MethodCallFailed>> {
    if !matches!(info.invoke_kind, 0 | 2) {
        return None;
    }
    let Some(Value::Object(Some(receiver))) = values.first().copied() else {
        return None;
    };
    // An array of lambda proxies (`Array.newInstance(r.getClass(), n)`) carries
    // the proxy's id in its header; it dispatches on `Object`, not the SAM.
    let receiver_class_id = jit_dispatch_class(vm, receiver).class_id()?;
    if !vm.classes.is_lambda_proxy_class(receiver_class_id) {
        return None;
    }
    match crate::runtime::interpreter::try_lambda_dispatch(
        vm,
        thread,
        receiver,
        receiver_class_id,
        info.method_name,
        info.descriptor,
        &values[1..],
    ) {
        Ok(Some(result)) => Some(Ok(result)),
        // Not the proxy's SAM (a default method on the functional interface,
        // say) — fall through to the caller's ordinary path, unchanged.
        Ok(None) => None,
        Err(error) => Some(Err(error)),
    }
}

/// The class a JIT helper DISPATCHES a receiver on, read from its header.
///
/// Round 11 wave 13 (lane `icwide`; the "Siblings checked" half of
/// `docs/feature-designs/real-jdk-mode.md`).
/// An array's header carries its COMPONENT's class id, so a bare
/// `class_id_of` takes an `Integer[]` for an `Integer`, a `K[]` for a `K`, and
/// an array of lambda proxies for a lambda proxy. An array dispatches on
/// `java/lang/Object` (JVMS §4.4.1 / §5.4.6), and no class-id key can name
/// that, so [`Self::class_id`] answers `None` for it and the caller declines
/// to its resolving path. Every class-id-keyed cache, guard or resolution in
/// this file reads its receiver through [`jit_dispatch_class`] /
/// [`jit_dispatch_class_validated`], so the kind screen cannot be forgotten
/// at the next one.
#[derive(Clone, Copy)]
pub enum JitDispatchClass {
    /// A plain object: the header's class id is its runtime class.
    Class(ClassId),
    /// An array (or any other non-plain header): `java/lang/Object`'s methods.
    JavaLangObject,
}

impl JitDispatchClass {
    /// The id to key a cache or guard on; `None` for `java/lang/Object`
    /// dispatch (an array), which such a key cannot describe.
    #[inline]
    pub fn class_id(self) -> Option<ClassId> {
        match self {
            Self::Class(cid) => Some(cid),
            Self::JavaLangObject => None,
        }
    }
}

/// [`JitDispatchClass`] of `obj` through the validating header reads
/// (`kind_of` / `class_id_of`); for a receiver not just proved by
/// `is_object_address`.
#[inline]
pub fn jit_dispatch_class(vm: &SharedVm, obj: ObjectRef) -> JitDispatchClass {
    if vm.mem.heap.kind_of(obj) != cratonvm_types::ObjectKind::Object {
        return JitDispatchClass::JavaLangObject;
    }
    JitDispatchClass::Class(vm.mem.heap.class_id_of(obj))
}

/// [`jit_dispatch_class`] for an `obj` the caller has JUST obtained from
/// `is_object_address` (the `class_id_of_validated` contract).
#[inline]
pub fn jit_dispatch_class_validated(vm: &SharedVm, obj: ObjectRef) -> JitDispatchClass {
    if vm.mem.heap.kind_of_validated(obj) != cratonvm_types::ObjectKind::Object {
        return JitDispatchClass::JavaLangObject;
    }
    JitDispatchClass::Class(vm.mem.heap.class_id_of_validated(obj))
}

pub struct VirtualDispatchTarget {
    pub class_name: std::sync::Arc<str>,
    pub cacheable_receiver: bool,
}

/// Resolve the runtime dispatch class for a virtual/interface helper call.
/// `invoke_or_native` binds to the class name it is handed rather than
/// re-dispatching on the receiver, so the helper must hand it the receiver's
/// *runtime* class when that class is trustworthy, or the CP-resolved
/// call-site class when the receiver carries only an unusable/synthetic id.
///
/// `cacheable_receiver` is true only when `class_name` is the receiver class
/// named by the MIC/PIC key. If the receiver is `ClassId(0)`, an array, or a
/// bare/interface fallback to the CP class, publishing that result under the
/// receiver id would poison later inline-cache hits.
///
/// Residual-6 diagnosis (env-gated, `CRATONVM_TRACE_CLASSVALUE`): true when
/// the `ClassValue.get(Class)` dispatch-trace probes should fire. Cached so
/// the hot dispatch path pays two slice compares + one bool load.
pub fn cv_trace_enabled() -> bool {
    pub static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || false)}

/// Residual-6 diagnosis: does this invoke-info describe the
/// `get(Ljava/lang/Class;)Ljava/lang/Object;` signature the `ClassValue`
/// native answers? (Class-blind on purpose — the probe wants every route.)
pub fn cv_trace_match(info: &JitInvokeInfo) -> bool {
    // `CRATONVM_DBG_MIC_METHOD=<substring>` retargets this trace at any method.
    // It was hardcoded to one investigation's `get(Class)Object` site, which
    // made it useless for the next one; the MIC is the single hardest place in
    // the VM to observe, so the instrument should not need a rebuild to move.
    if let Some(want) = mic_trace_method_filter() {
        return info.method_name.contains(want);
    }
    // The latched flag first (r9w5): every compiled `get` call — `List.get`,
    // `Map.get` — reaches this on each `jit_invoke_virtual_mic`, and with the
    // trace off (always, outside an investigation) one cached bool answers it
    // without the two string compares.
    cv_trace_enabled()
        && info.method_name == "get"
        && info.descriptor == "(Ljava/lang/Class;)Ljava/lang/Object;"
}

/// `CRATONVM_DBG_MIC_METHOD` — retarget [`cv_trace_match`] at an arbitrary
/// method name (substring match).
pub fn mic_trace_method_filter() -> Option<&'static str> {
    use std::sync::OnceLock;
    pub static F: OnceLock<Option<String>> = OnceLock::new();
    F.get_or_init(|| Err::<String, std::env::VarError>(std::env::VarError::NotPresent).ok())
        .as_deref()}

// SAFETY: `vm` must be live for the call, and `receiver` must be a valid
// heap reference — it is dereferenced through `kind_of`/`class_id_of` to read
// the object header. Callers reach this only from a JIT dispatch site that has
// already decoded the receiver from an operand slot, so it is non-null and has
// not been moved by a collection in the interval.
pub unsafe fn virtual_dispatch_target_for_receiver(
    vm: &SharedVm,
    receiver: ObjectRef,
    info: &JitInvokeInfo,
) -> VirtualDispatchTarget {
    // An array-typed call site resolves against `java.lang.Object`'s method
    // table by JVMS §4.4.1, whatever the receiver's header says — decide it from
    // the SITE, above the header check below. See the matching comment in
    // `runtime::interpreter::invoke`'s `execute_invoke_kind`: the header-driven
    // form is correct only while the header is trustworthy, and a
    // reclaimed-and-re-served block's is not.
    let dispatch = if info.class_name.starts_with('[') {
        JitDispatchClass::JavaLangObject
    } else {
        jit_dispatch_class(vm, receiver)
    };
    let Some(cid) = dispatch.class_id() else {
        return VirtualDispatchTarget {
            class_name: std::sync::Arc::from("java/lang/Object"),
            cacheable_receiver: false,
        };
    };
    if cid == ClassId::new(0) {
        return VirtualDispatchTarget {
            class_name: if crate::vm::is_object_member(info.method_name, info.descriptor) {
                std::sync::Arc::from("java/lang/Object")
            } else {
                std::sync::Arc::from(info.class_name)
            },
            cacheable_receiver: false,
        };
    }

    let cm = vm.classes.class_manager.read();
    let Some(recv_class) = cm.get_class(cid) else {
        // A lambda proxy is not in the class store. A call that is not its SAM
        // (the lambda arm declined it) is a default method of the functional
        // interface, or an abstract method of one of its super-interfaces that
        // the interface implements with a default (`interface Throwing<F, R>
        // extends Function<F, R> { default R apply(F f) {..} }`). Resolving the
        // by-name tail against the SITE's class (`Function`) invoked the
        // abstract `Function.apply` on the proxy, which answered its first
        // argument; the interpreter selects from the proxy's own interface.
        // Quarkus suite wave 1 (`JandexUtilTest`: AssertJ `extracting(..)`).
        let class_name = vm
            .classes
            .lambda_call_site_for(cid)
            .filter(|_| !crate::vm::is_object_member(info.method_name, info.descriptor))
            .map(|site| site.functional_interface.clone())
            .unwrap_or_else(|| std::sync::Arc::from(info.class_name));
        return VirtualDispatchTarget {
            class_name,
            cacheable_receiver: false,
        };
    };
    let recv_name = recv_class.name.clone();
    let recv_is_iface = recv_class.is_interface();
    let recv_is_bare_object = recv_name.as_ref() == "java/lang/Object";
    let cp_is_not_object = info.class_name != "java/lang/Object";
    if (recv_is_iface || recv_is_bare_object)
        && cp_is_not_object
        && !crate::vm::is_object_member(info.method_name, info.descriptor)
        && recv_name.as_ref() != info.class_name
    {
        // COL8-2: an instance minted on a sub-interface of the site's class
        // that declares the method runs its own (published nowhere).
        let own = recv_is_iface
            && interface_receiver_dispatches_on_itself(
                vm,
                &cm,
                cid,
                jit_site_owner(&cm, info),
                info.class_name,
                info.method_name,
                info.descriptor,
            );
        VirtualDispatchTarget {
            class_name: if own {
                recv_name
            } else {
                std::sync::Arc::from(info.class_name)
            },
            cacheable_receiver: false,
        }
    } else if let Some((selected, _)) = receiver_selection_retarget(&cm, cid, info) {
        // The receiver's class would run a body §5.4.6 does not select (see
        // `receiver_selection_retarget`): dispatch by the selected class's
        // name, and publish nothing under the receiver's id.
        VirtualDispatchTarget {
            class_name: selected,
            cacheable_receiver: false,
        }
    } else {
        VirtualDispatchTarget {
            class_name: recv_name,
            cacheable_receiver: true,
        }
    }
}

/// The class a compiled call site's constant pool names (`info.class_name`),
/// looked up through the site's own loader (`info.declaring_class_id`). `None`
/// for an array owner, a class not loaded there, and a site with no context
/// (`declaring_class_id == 0`: the synthetic direct-dispatch sites, which name
/// JDK classes with one definition and need no selection — and whose by-name
/// bootstrap lookup would allocate on every uncached dispatch). Loads nothing.
pub fn jit_site_owner(cm: &crate::classloading::ClassManager, info: &JitInvokeInfo) -> Option<ClassId> {
    if info.class_name.starts_with('[') || info.declaring_class_id == 0 {
        return None;
    }
    cm.find_cp_class_for_class(info.class_name, ClassId::new(info.declaring_class_id))
}

/// JVMS §5.4.6 for a compiled virtual/interface call dispatched on the
/// receiver's class: the NAME of the class whose declaration the call runs,
/// when the lenient walk from `receiver_cid` would run another one — a private
/// or static method hiding the inherited one, or a public method in another
/// package that does not override a package-private resolved method. The
/// by-name dispatch (`invoke_or_native`) then starts at the selected class and
/// finds its own declaration first.
///
/// `None` — the usual answer, and the only one for a hierarchy with a
/// compatibility class in it or for §5.4.6's error answers — keeps the
/// receiver's class; see `resolve::selection::walk_selection_moves_to`.
///
/// Returns the selected class's id with its name (MEGA3-17-4), so a caller
/// that compiles for it (`CachedDispatchTarget::selected_cid`) needs no
/// by-name lookup. The name need not round-trip through the global table
/// (round 17 wave 3, lane select, MEGA3-17-2,
/// `r17-mega3-retarget-dropped-for-an-ambiguous-selected-name-FIXED-20261005.md`):
/// the selected class is a supertype of the receiver, and the by-name tail
/// dispatches a supertype name on the receiver's own linked copy
/// (`by_name_tail_dispatch_class` in `vm/src/vm/vm_exec.rs`). Where it does
/// not round-trip, `CRATONVM_SELECTION_RECEIVER_SUPERTYPE=0` restores the old
/// `None` (the receiver's own, non-selected body ran).
///
/// Asked where a dispatch target is resolved under the class-manager read
/// lock the caller already holds, which for the memoized form is once per
/// (site, receiver class).
pub fn receiver_selection_retarget(
    cm: &crate::classloading::ClassManager,
    receiver_cid: ClassId,
    info: &JitInvokeInfo,
) -> Option<(std::sync::Arc<str>, ClassId)> {
    let owner = jit_site_owner(cm, info)?;
    let store = cm.class_store();
    let selected = crate::runtime::resolve::selection::receiver_dispatch_moves_to(
        store,
        receiver_cid,
        owner,
        info.method_name,
        info.descriptor,
    )?;
    let name = store.get(selected)?.name.clone();
    let faithful = cm.get_loaded_class_id(&name) == Some(selected)
        || crate::vm::vm_exec::selection_receiver_supertype_enabled();
    faithful.then_some((name, selected))
}

/// [`crate::runtime::resolve::selection::select_for_receiver_dispatch`] for a
/// compiled call site: the method a receiver of class `receiver_cid` runs for
/// `info`'s method, where every resolver in this file used to answer with the
/// lenient walk from the receiver's class alone.
pub fn select_for_jit_receiver<'a>(
    cm: &'a crate::classloading::ClassManager,
    receiver_cid: ClassId,
    info: &JitInvokeInfo,
) -> Option<(&'a cratonvm_reader::method::ClassFileMethod, ClassId)> {
    crate::runtime::resolve::selection::select_for_receiver_dispatch(
        cm.class_store(),
        receiver_cid,
        jit_site_owner(cm, info),
        info.method_name,
        info.descriptor,
    )
}

/// A resolved virtual/interface dispatch target, memoized per thread by
/// [`virtual_dispatch_target_cached`].
///
/// `class_name` is an `Rc<str>` rather than the `Arc<str>` the uncached
/// resolver returns, and that is deliberate: the memo lives in a thread-local
/// and every consumer only ever needs `&str`, so a cache hit must not touch a
/// refcount shared with other threads. An `Arc::clone` here would be an atomic
/// RMW on one class's refcount from every mutator at once — the same contended
/// cache line this memo exists to stop touching.
#[derive(Clone)]
pub struct CachedDispatchTarget {
    pub class_name: std::rc::Rc<str>,
    pub cacheable_receiver: bool,
    /// `get_loaded_class_id(class_name) == Some(receiver_cid)` — the name
    /// round-trips back to exactly this receiver class, so resolving a callee
    /// *by name* cannot land in another loader's same-named class. Only
    /// meaningful when `cacheable_receiver`. No longer a publication or
    /// compile gate (interpreter round i1 wave 7): every door that used it now
    /// compiles for the receiver's class id; kept for the `cv-mic` trace.
    pub globally_named: bool,
    /// JVMS §5.4.6 selection for this (site, receiver class) ENDS IN AN ERROR
    /// (`AbstractMethodError` / conflicting defaults' ICCE) that the lenient
    /// walk from the receiver would answer with a body. Such a target is never
    /// `cacheable_receiver` (no compiled entry may be published for it), and
    /// every door that reads it raises [`jit_selection_failure`] instead of
    /// dispatching by name (round 12 wave 8 orchestrator,
    /// `r12w8-orch-probe-findings-on-w8` item 1: compiled callers ran the
    /// lenient body where the interpreter threw). Also set, under
    /// `--real-jdk`, for an `invokeinterface` whose receiver does not
    /// implement the interface (`jit_receiver_does_not_implement`, JVMS §6.5
    /// `IncompatibleClassChangeError`; interpreter round i1 wave 38, lane L6).
    pub selection_error: bool,
    /// The class id JVMS §5.4.6 selection moved this receiver's dispatch to
    /// (`receiver_selection_retarget`: a package-private resolved method next
    /// to a non-overriding public one in another package), when the target
    /// may be compiled for THAT class and published under the RECEIVER's id;
    /// `0` otherwise. Set only under `CRATONVM_JIT_MEGA_CACHE_RETARGETED`
    /// ([`mega_cache_retargeted_enabled`]; round 17 wave 2, lane mega2,
    /// MEGA17-3), and never together with `cacheable_receiver` or
    /// `selection_error`. Sound because the selected body is a function of
    /// (the site's resolution, the receiver class), which is exactly what
    /// every cache it is published into is keyed by: the site's own MIC/PIC
    /// ways, the per-thread entry cache keyed (site, receiver class id), and
    /// the shared table keyed (selector, receiver class id), whose selector
    /// names the resolved class, method and loader context. So it can never
    /// answer for a receiver whose selection differs.
    pub selected_cid: u32,
}

impl CachedDispatchTarget {
    /// May a compiled entry be published for this (site, receiver class)
    /// target: a receiver-named one, or one whose §5.4.6 selection was
    /// retargeted ([`Self::selected_cid`]). The one publication condition of
    /// the MIC arms and the blind arm (j27 wave 4, lane DISPROWS, MEGA3-17-3).
    pub fn publishable(&self) -> bool {
        self.cacheable_receiver || self.selected_cid != 0
    }

    /// The class id the published body is compiled for: the selected class
    /// of a retargeted receiver, else the receiver's own `receiver_cid`.
    pub fn compile_cid(&self, receiver_cid: u32) -> u32 {
        if self.selected_cid != 0 {
            self.selected_cid
        } else {
            receiver_cid
        }
    }
}

/// `CRATONVM_JIT_MEGA_CACHE_RETARGETED` (default ON since the round-17 close,
/// `=0` off; A/B in jit-optin-switch-triage-r17-CLOSED-20261006.md; round 17 wave 2,
/// lane mega2, MEGA17-3,
/// `r17-mega-selection-retargeted-receivers-are-never-cached-FIXED-20261005.md`).
/// With it on, a receiver whose §5.4.6 selection moves to a superclass gets
/// its SELECTED class's compiled body published under its own class id (see
/// [`CachedDispatchTarget::selected_cid`]) instead of paying a by-name Rust
/// dispatch on every call. Read only where a (site, receiver class) target is
/// resolved on a memo miss, and only for a retargeted receiver.
pub fn mega_cache_retargeted_enabled() -> bool {
    true
}

/// COL8-2 (round 17 wave 2, lane mega2): may a receiver whose runtime class
/// is itself an INTERFACE dispatch on that interface rather than on the call
/// site's constant-pool class? The S111r12 fallback (here and in the
/// interpreter's `execute_invoke_kind`) sends every interface-classed receiver
/// to the constant-pool class, which is right for the case it was written for
/// (a receiver whose class id names an unrelated interface) and wrong for an
/// instance minted ON a sub-interface of the site's interface: the
/// `--compatible` `IntStream.iterator()` snapshot is a `PrimitiveIterator$OfInt`
/// and `invokeinterface Iterator.next` ran `Iterator.next`'s native instead of
/// `OfInt.next`'s (`R16CollectionsPrimItrMin`), while every cached door ran
/// the receiver's own. True when the receiver interface is a subtype of the
/// site's class AND declares the method itself, with a body or a registered
/// native, which is what §5.4.6 selection from that class would find first.
/// `CRATONVM_IFACE_RECEIVER_OWN_DISPATCH=0` restores the fallback. The switch
/// is read last, only for such a receiver.
pub fn interface_receiver_dispatches_on_itself(
    vm: &SharedVm,
    cm: &crate::classloading::ClassManager,
    receiver_cid: ClassId,
    site_owner: Option<ClassId>,
    site_class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let Some(recv_class) = cm.get_class(receiver_cid) else {
        return false;
    };
    if !recv_class.is_interface() || recv_class.name.as_ref() == site_class_name {
        return false;
    }
    let is_subtype = match site_owner {
        Some(owner) => cm.is_subclass_of(receiver_cid, owner),
        None => cm.is_assignable_to_name(receiver_cid, site_class_name),
    };
    if !is_subtype {
        return false;
    }
    let declares = recv_class
        .find_method(method_name, descriptor)
        .is_some_and(|m| m.code().is_some())
        || vm
            .natives
            .native_methods
            .find(&recv_class.name, method_name, descriptor)
            .is_some();
    declares
}

/// The error a [`CachedDispatchTarget::selection_error`] target raises, built
/// on the (rare) call that meets it. `None` when the switch
/// (`CRATONVM_NATIVE_INVOKE_STRICT_SELECTION` under `--compatible`) is off or
/// the answer changed; the caller then dispatches as before.
pub fn jit_selection_failure(
    vm: &SharedVm,
    thread: &mut JvmThread,
    receiver: ObjectRef,
    info: &JitInvokeInfo,
) -> Option<crate::error::MethodCallFailed> {
    // JVMS 5.4.3.3 / 5.4.3.4 before §6.5 and §5.4.6: a site whose own
    // reference does not resolve raises its `NoSuchMethodError` whatever the
    // receiver (wave 100, lane L2; `virtual_dispatch_target_cached` flags it).
    let unresolvable_owner = {
        let cm = vm.classes.class_manager.read();
        jit_site_unresolvable_owner(vm, &cm, info)
    };
    if let Some(owner_name) = unresolvable_owner {
        return Some(jit_site_unresolvable_error(vm, thread, owner_name, info));
    }
    let Some(receiver_cid) = jit_dispatch_class(vm, receiver).class_id() else {
        // An array receiver (`virtual_dispatch_target_cached`'s array arm).
        let message = jit_array_receiver_does_not_implement(vm, receiver, info)?;
        return Some(crate::runtime::exceptions::throw_linkage_error(
            vm,
            thread,
            crate::error::LinkageError::IncompatibleClassChangeError { message },
        ));
    };
    if receiver_cid == ClassId::new(0) {
        // A plain `java.lang.Object` (`virtual_dispatch_target_cached`'s
        // class-id-0 arm); nothing else is flagged for class id 0.
        let message = jit_object_receiver_does_not_implement(vm, receiver, info)?;
        return Some(crate::runtime::exceptions::throw_linkage_error(
            vm,
            thread,
            crate::error::LinkageError::IncompatibleClassChangeError { message },
        ));
    }
    let (owner, not_implemented) = {
        let cm = vm.classes.class_manager.read();
        (
            jit_site_owner(&cm, info),
            jit_receiver_does_not_implement(vm, &cm, receiver_cid, info),
        )
    };
    // JVMS §6.5 `invokeinterface`, checked before selection as HotSpot does
    // (`LinkResolver::runtime_resolve_interface_method`).
    if let Some(message) = not_implemented {
        return Some(crate::runtime::exceptions::throw_linkage_error(
            vm,
            thread,
            crate::error::LinkageError::IncompatibleClassChangeError { message },
        ));
    }
    let owner = owner?;
    // j27 wave 9 (lane W9EXC): a COMPILED caller raises the resolution text
    // (`Receiver class X does not define or inherit an implementation of the
    // resolved method ... Selected method is 'abstract ...'.`) even at a
    // linked `invokevirtual` site: HotSpot's compiled code reaches an abstract
    // selection through `SharedRuntime` (IC miss / `handle_wrong_method_abstract`),
    // which passes the receiver to `LinkResolver::throw_abstract_method_error`.
    // Measured on HotSpot 25.0.3 (`J27DispatchSelectRows`
    // `watermark-abstract-redecl-msg`): default, C1-only and C2-only answer the
    // receiver text; only `-Xint` answers the linked vtable-entry text, which
    // the interpreter keeps (`CRATONVM_LINKED_INVOKEVIRTUAL_AME_TEXT`).
    // Not `reflective_selection_failure`: its same-class fast accept answered
    // `None` for a receiver of exactly the site's class, and the site then ran
    // the lenient body (wave 35).
    crate::vm::vm_exec::flagged_selection_failure(
        vm,
        thread,
        receiver_cid,
        owner,
        info.method_name,
        info.descriptor,
    )
}

/// Does the call `info` end in an error for a receiver of class `receiver_cid`
/// that the lenient walk would answer with a body: an `invokeinterface` on a
/// receiver that does not implement the interface
/// ([`jit_receiver_does_not_implement`]), or a §5.4.6 selection error
/// ([`jit_strict_selection_errs`])? For the doors that ask before building a
/// callee template, under the caller's read guard.
pub fn jit_selection_errs(
    vm: &SharedVm,
    cm: &crate::classloading::ClassManager,
    receiver_cid: ClassId,
    info: &JitInvokeInfo,
) -> bool {
    // Resolution of the site's own reference first (wave 100, lane L2): an
    // unresolvable one is the `NoSuchMethodError` whatever the receiver.
    jit_site_unresolvable_owner(vm, cm, info).is_some()
        || jit_receiver_does_not_implement(vm, cm, receiver_cid, info).is_some()
        || jit_strict_selection_errs(cm, receiver_cid, info)
}

/// JVMS §6.5 `invokeinterface` under `--real-jdk`: HotSpot's
/// `IncompatibleClassChangeError` message when `receiver_cid` provably does
/// not implement the interface the site `info` names
/// ([`crate::runtime::resolve::selection::receiver_does_not_implement`], the
/// interpreter's check since wave 30).
///
/// Asked only where a (site, receiver class) target is resolved: the miss
/// path of [`virtual_dispatch_target_cached`], which memoizes the answer, and
/// the bytecode-callee door's cold fill. A receiver it flags is never
/// `cacheable_receiver`, so no inline cache, entry memo or shared
/// megamorphic table is ever filled for it and no hit path carries the check
/// (interpreter round i1 wave 38, lane L6; `L6W38InterfaceReceiverHot`).
/// Kind 2 only: `invokevirtual` cannot name an interface, and a private
/// interface method pinned as kind 1 is not compiled under `--real-jdk`
/// (wave 37). `--compatible` keeps its duck typing.
pub fn jit_receiver_does_not_implement(
    vm: &SharedVm,
    cm: &crate::classloading::ClassManager,
    receiver_cid: ClassId,
    info: &JitInvokeInfo,
) -> Option<String> {
    if info.invoke_kind != 2 || !vm.classes.selection_checks {
        return None;
    }
    let owner = jit_site_owner(cm, info)?;
    crate::runtime::resolve::selection::receiver_does_not_implement(cm, receiver_cid, owner)
}

/// [`jit_receiver_does_not_implement`] for a receiver whose header carries
/// class id 0: a plain `java.lang.Object` has it
/// ([`crate::runtime::resolve::selection::object_receiver_does_not_implement`]
/// says when it is one), and the dispatch helpers otherwise treat the id as
/// "no usable class" and call by the interface's name. Takes the
/// class-manager lock itself; asked only for a class-id-0 receiver at an
/// `invokeinterface` site under `--real-jdk`, never on a hit path.
pub fn jit_object_receiver_does_not_implement(
    vm: &SharedVm,
    receiver: ObjectRef,
    info: &JitInvokeInfo,
) -> Option<String> {
    if info.invoke_kind != 2 || !vm.classes.selection_checks {
        return None;
    }
    let slots = vm.mem.heap.num_fields(receiver);
    if slots != 0 {
        return None;
    }
    let cm = vm.classes.class_manager.read();
    let owner = jit_site_owner(&cm, info)?;
    crate::runtime::resolve::selection::object_receiver_does_not_implement(&cm, slots, owner)
}

/// [`jit_receiver_does_not_implement`] for an ARRAY receiver
/// ([`crate::runtime::resolve::selection::array_receiver_does_not_implement`]):
/// the dispatch helpers answer one with `java/lang/Object` before any class
/// check, so an `invokeinterface` of an interface's default method on an
/// `int[]` ran the default, where HotSpot raises the ICCE (interpreter round
/// i1 wave 39, lane L2; item 1 of `i38-L6-compiled-dispatch-leftovers`).
/// `--real-jdk`, kind 2, not an array-typed site; a site naming `Cloneable`
/// or `Serializable` answers before any lock. Never on a hit path: an array
/// receiver has no class-id key, so every call of one takes the resolving
/// path this is asked on.
pub fn jit_array_receiver_does_not_implement(
    vm: &SharedVm,
    receiver: ObjectRef,
    info: &JitInvokeInfo,
) -> Option<String> {
    if info.invoke_kind != 2
        || !vm.classes.selection_checks
        || info.class_name.starts_with('[')
        || crate::runtime::resolve::selection::array_interface_name(info.class_name)
        || vm.mem.heap.kind_of(receiver) != cratonvm_types::ObjectKind::Array
    {
        return None;
    }
    let descriptor = crate::runtime::interpreter::array_descriptor_of(vm, receiver)?;
    let cm = vm.classes.class_manager.read();
    let owner = jit_site_owner(&cm, info)?;
    crate::runtime::resolve::selection::array_receiver_does_not_implement(&cm, &descriptor, owner)
}

/// Does §5.4.6 selection of `info`'s method for `receiver_cid` end in an
/// error the lenient walk would answer with a body? For
/// [`CachedDispatchTarget::selection_error`], under the caller's read guard.
pub fn jit_strict_selection_errs(
    cm: &crate::classloading::ClassManager,
    receiver_cid: ClassId,
    info: &JitInvokeInfo,
) -> bool {
    let Some(owner) = jit_site_owner(cm, info) else {
        return false;
    };
    crate::runtime::resolve::selection::receiver_selection_error(
        cm.class_store(),
        receiver_cid,
        owner,
        info.method_name,
        info.descriptor,
    )
    .is_some()
}

/// Clear-on-full bound for [`VIRTUAL_TARGET_CACHE`]. Entries are per
/// (call site, receiver class) pair, so a long-running server with a large
/// megamorphic surface could otherwise grow this without limit. Losing the
/// memo just reverts to the (correct, slower) locked resolution — same
/// bounded-cache posture as `ClassManager::note_synthetic_upgrade_absent`.
pub const VIRTUAL_TARGET_CACHE_CAP: usize = 4096;

/// Identity of one JIT call site, for every per-thread dispatch memo in this
/// module: `(SharedVm::vm_identity, JitInvokeInfo pointer)`.
///
/// **The `vm_identity` half is load-bearing — do not drop it.** A
/// `JitInvokeInfo` pointer alone does NOT identify a call site across VMs:
///
///  * several call sites pass the address of a process-global `static
///    JitInvokeInfo` (`INTEGER_VALUE_OF_INFO`, `INTEGER_INT_VALUE_INFO`,
///    `HASHMAP_PUT_DIRECT_INFO`, `HASHMAP_GET_DIRECT_INFO`,
///    `CONCURRENT_HASHMAP_GET_DIRECT_INFO`, `STRING_LATIN1_LOWER_DIRECT_INFO`),
///    which is literally the SAME address in every VM in the process;
///  * every other info is owned by the `CompiledMethod` that bakes it
///    (`_jit_invoke_infos`) and freed with that artifact, so a later
///    compilation — in this VM or another — can be handed the same address.
///
/// What these memos hold is per-VM to the word: raw compiled entry pointers
/// into one VM's code cache, receiver `ClassId`s, `NativeMethodId` census
/// handles from one VM's registry, and resolved callee class names. A
/// cross-VM hit therefore does not degrade to a slow path — it CALLs another
/// VM's compiled body, or runs `java/util/HashMap`'s native against whatever
/// class happens to hold that id in this VM. See
/// `vm-jit-cache-keying.md`.
///
/// `vm_identity` is a monotonically issued counter (`vm_init.rs`
/// `NEXT_VM_IDENTITY`), never an address, so it is never recycled — unlike a
/// `&SharedVm` cast to `usize`, which a sequentially-created second VM can
/// plausibly inherit from a dropped one.
pub type JitSiteKey = (usize, usize);

/// Build a [`JitSiteKey`]. Free function (not a method on `SharedVm`) so the
/// keying can be unit-tested without constructing a VM.
#[inline]
pub fn jit_site_key(vm_identity: usize, info_ptr: usize) -> JitSiteKey {
    (vm_identity, info_ptr)
}

thread_local! {
    /// `(vm_identity, that VM's class-store definition epoch,
    /// any_class_redefined)` this thread last flushed its class-identity
    /// dispatch memos at. See [`flush_class_identity_dispatch_memos`].
    /// `vm_identity` 0 is never a real VM, so the first call flushes.
    pub static DISPATCH_MEMO_CLASS_IDENTITY: Cell<(usize, u64, bool)> =
        const { Cell::new((0, 0, false)) };
    /// zt t3/nd: the narrow key of [`flush_class_identity_dispatch_memos`]
    /// under `CRATONVM_JIT_DISPATCH_MEMO_NAME_EPOCH`: `(vm_identity, name
    /// generation, hierarchy-edge epoch, in-place origin epoch,
    /// any_class_redefined)`, and the definition epoch the refusal entries
    /// were last dropped at. `vm_identity` 0 never matches.
    pub static DISPATCH_MEMO_CLASS_NAMES: Cell<((usize, u64, u64, u64, bool), u64)> =
        const { Cell::new(((0, 0, 0, 0, false), 0)) };
    /// `(vm_identity, redefine epoch)` at which this thread last ran the
    /// quiesced-site flush in `jit_invoke_dispatch_body`. `u32::MAX` never
    /// matches a real epoch before four billion redefinitions.
    pub static QUIESCED_SITE_FLUSH_STAMP: Cell<(usize, u32)> = const { Cell::new((0, u32::MAX)) };
}

/// Flush this thread's dispatch memos whose validity depends on class
/// *identity* — the name-to-`ClassId` mapping — as opposed to compiled-code
/// lifetime.
///
/// Two atomic loads on the steady-state path, and nothing else.
///
/// `class_definition_epoch` moves whenever a class is defined or unloaded,
/// which is precisely what can invalidate a memoized `globally_named` answer.
/// None of the signals `jit_invoke_dispatch` already checks covers that:
/// a second loader defining the same name redefines nothing, publishes no
/// compiled code, and supersedes no tier. So clearing `DISPATCH_CACHE` and
/// `VIRTUAL_DISPATCH_CACHE` here also closes a hole those two already had —
/// they cache a compiled entry chosen on the strength of a `globally_named`
/// test that nothing was re-running.
///
/// `STATIC_BYTECODE_CALLEE_CACHE` is here for the same reason and needs it
/// most: it holds an interpreter frame template resolved through
/// `get_loaded_class_id(info.class_name)`, i.e. keyed on a class NAME. A second
/// loader defining that name is precisely the event that makes the template
/// describe the wrong class's method — and it is invisible to the redefine and
/// tier signals, so without this line the site would keep entering the first
/// loader's body forever.
///
/// `any_class_redefined` is folded in as cheap insurance. None of the other
/// memoized fields can actually move under redefinition (a `ClassId`'s name
/// and `ACC_INTERFACE` are fixed at definition, and redefinition rewrites
/// method bodies in place), but it is a sticky bool whose single false->true
/// transition costs one flush for the life of the process, so paying it is
/// strictly cheaper than reasoning about every future redefinition path.
///
/// The epoch is `vm`'s class store's (`StoreEpochs`, interpreter round i1
/// wave 21, lane L5): every memo flushed here is keyed by `vm_identity`, and
/// a class defined in another VM cannot change what a name means in `vm`, so
/// it no longer empties this thread's memos. The state names the VM too: a
/// thread that switches VMs flushes, so every entry left in the memos was
/// filled by the VM the state names, under the epoch it records.
/// Release what THIS thread's memos hold of the VM `vm_identity`'s compiled
/// code, when that VM is dropped (`Drop for Vm`).
///
/// The site-keyed memos (`DispatchCache::owner`), [`LAMBDA_GET_MEMO`]'s direct
/// routes and the lambda site caches each own a compiled body. They are per
/// thread and keyed by VM identity, and nothing else empties them for a VM
/// that is gone: the site-keyed ones are flushed only when this thread next
/// dispatches into some VM, and the lambda ones never. So a thread that
/// outlived its VM kept that VM's bodies -- and the lambda thunks their inline
/// caches own -- mapped for as long as it lived
/// (`docs/feature-designs/jit-ir-optimizations.md`).
///
/// The site-keyed memos are emptied whole, every VM's rows: they are caches,
/// a VM drop is rare, and a living VM's rows refill on their next miss. A
/// released owner goes through the retirement queue (`RetainedCode`'s drop).
/// Other threads' memos are theirs to drop: a thread of the dropped VM ends
/// with it, and its TLS with it.
pub fn release_thread_jit_memos_for_vm(vm_identity: usize) {
    clear_site_keyed_dispatch_memos();
    LAMBDA_GET_MEMO.with(|m| {
        if let Ok(mut map) = m.try_borrow_mut() {
            map.retain(|key, _| key.0 != vm_identity);
        }
    });
    crate::runtime::interpreter::lambda::release_thread_lambda_sites_for_vm(vm_identity);
    // W3B: the interpreter's virtual-door body memo (cross-lane, one line).
    crate::runtime::interpreter::release_virtual_body_memo();
}

pub fn flush_class_identity_dispatch_memos(vm: &SharedVm) {
    if dispatch_memo_name_epoch_enabled() {
        flush_class_identity_dispatch_memos_narrow(vm);
        return;
    }
    let state = (
        vm.vm_identity,
        cratonvm_classloading::store_epochs(vm.jit.class_layout_domain).class_definition_epoch(),
        crate::classloading::any_class_redefined(),
    );
    DISPATCH_MEMO_CLASS_IDENTITY.with(|seen| {
        if seen.get() != state {
            seen.set(state);
            VIRTUAL_TARGET_CACHE.with(|c| c.borrow_mut().clear());
            DISPATCH_CACHE.with(|c| c.borrow_mut().clear());
            VIRTUAL_DISPATCH_CACHE.with(|c| c.borrow_mut().clear());
            STATIC_BYTECODE_CALLEE_CACHE.with(|c| c.borrow_mut().clear());
            VIRTUAL_BYTECODE_CALLEE_CACHE.with(|c| c.borrow_mut().clear());
            SPECIAL_BYTECODE_CALLEE_CACHE.with(|c| c.borrow_mut().clear());
            // Resolved through `get_loaded_class_id(info.class_name)` too.
            WRAPPED_STATIC_CALLEE.with(|c| c.borrow_mut().clear());
            CALLEE_HANDLER_TEMPLATE_CACHE.with(|c| c.borrow_mut().clear());
        }
    });
}

/// zt t3/nd (`proxy-define-row-cpu-is-class-file-api-java-and-define-epoch-flushes-20261016-RETIRED-20261024.md`, the
/// per-define cost): `CRATONVM_JIT_DISPATCH_MEMO_NAME_EPOCH` (default ON since
/// g1gen, `=0` kill; read once per process). The class-definition epoch moves on EVERY
/// class definition, so a loop that defines a class per iteration (a fresh
/// `Proxy.getProxyClass` loader, a `defineHiddenClass` per call) emptied every
/// memo of [`flush_class_identity_dispatch_memos`] on every compiled call
/// after each define. A brand-new name cannot change a POSITIVE entry: each
/// holds a resolution of a name that was already mapped (it changes only with
/// the name generation: a second definer of an existing name, a removal, an
/// unload), a selection over stored classes' edges (the hierarchy-edge epoch,
/// which every forward edge also moves) and method tables (the in-place
/// origin epoch: a stub upgraded to real bytes), or a redefinition (the
/// sticky flag). A brand-new name CAN turn a cached REFUSAL into an answer,
/// so on a definition-epoch move alone the refusals are dropped (`None`
/// entries of the three bytecode-callee memos, and `WRAPPED_STATIC_CALLEE`,
/// which holds only refusals) and the positive entries stay.
///
/// Not every refusal-like entry is dropped: `VIRTUAL_TARGET_CACHE`'s
/// `selection_error: true` rows (unresolvable owner, receiver does not
/// implement, strict selection error) are KEPT. Each is a function of the
/// owner's or the receiver's stored hierarchy and method tables, which a
/// brand-new class cannot change without moving one of the four keyed
/// epochs. `DISPATCH_CACHE`, `VIRTUAL_DISPATCH_CACHE` and
/// `CALLEE_HANDLER_TEMPLATE_CACHE` hold positive entries only.
///
/// zt t6/rv3 (`zgc-zt4x3-*` item 1): read through a
/// [`cratonvm_types::flags::MemoSlot`] (was a process `OnceLock`, which a
/// test's thread override could never reach); an override resets the slot.
pub fn dispatch_memo_name_epoch_enabled() -> bool {
    pub static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// [`flush_class_identity_dispatch_memos`] under
/// [`dispatch_memo_name_epoch_enabled`]: the full flush when a name answer,
/// an edge, an in-place origin or the redefinition flag moved (or the thread
/// switched VMs); only the refusals when nothing but the definition epoch
/// moved. Five atomic loads on the steady state.
pub fn flush_class_identity_dispatch_memos_narrow(vm: &SharedVm) {
    let epochs = cratonvm_classloading::store_epochs(vm.jit.class_layout_domain);
    let state = (
        vm.vm_identity,
        epochs.class_name_generation(),
        epochs.hierarchy_edge_epoch(),
        epochs.class_origin_in_place_epoch(),
        crate::classloading::any_class_redefined(),
    );
    let definition = epochs.class_definition_epoch();
    narrow_dispatch_memo_flush(state, definition);
}

/// The body of [`flush_class_identity_dispatch_memos_narrow`] given the VM's
/// current keyed `state` and `definition` epoch (zt t6/rv3: split out so a
/// unit test can move one epoch at a time without defining classes).
pub fn narrow_dispatch_memo_flush(state: (usize, u64, u64, u64, bool), definition: u64) {
    DISPATCH_MEMO_CLASS_NAMES.with(|seen| {
        let (seen_state, seen_definition) = seen.get();
        if seen_state != state {
            seen.set((state, definition));
            VIRTUAL_TARGET_CACHE.with(|c| c.borrow_mut().clear());
            DISPATCH_CACHE.with(|c| c.borrow_mut().clear());
            VIRTUAL_DISPATCH_CACHE.with(|c| c.borrow_mut().clear());
            STATIC_BYTECODE_CALLEE_CACHE.with(|c| c.borrow_mut().clear());
            VIRTUAL_BYTECODE_CALLEE_CACHE.with(|c| c.borrow_mut().clear());
            SPECIAL_BYTECODE_CALLEE_CACHE.with(|c| c.borrow_mut().clear());
            WRAPPED_STATIC_CALLEE.with(|c| c.borrow_mut().clear());
            CALLEE_HANDLER_TEMPLATE_CACHE.with(|c| c.borrow_mut().clear());
        } else if seen_definition != definition {
            seen.set((state, definition));
            STATIC_BYTECODE_CALLEE_CACHE.with(|c| c.borrow_mut().retain(|_, v| v.is_some()));
            VIRTUAL_BYTECODE_CALLEE_CACHE.with(|c| c.borrow_mut().retain(|_, v| v.is_some()));
            SPECIAL_BYTECODE_CALLEE_CACHE.with(|c| c.borrow_mut().retain(|_, v| v.is_some()));
            WRAPPED_STATIC_CALLEE.with(|c| c.borrow_mut().clear());
        }
    });
}


/// Was the class the call site `info` names ever redefined
/// ([`class_id_or_name_was_redefined`])? Memoized per site at its VM's
/// redefinition epoch (`QUIESCE_VERDICT`; round 12 wave 1, lane rt, the
/// quiesce-verdict memo of `r11-rt-helper-hot-paths-repeat-work-per-call`).
///
/// Once any class in the process was redefined (a Mockito inline mock, an
/// agent's retransform) the uncached verdict is a class-manager read lock and
/// a by-id-or-name lookup on EVERY `jit_invoke_dispatch_body` call, at every
/// site, redefined or not. The memo makes it one thread-local probe.
///
/// Sound because a `false` only turns `true` through a redefinition, and the
/// one production path (`redefine_class_with`) bumps the class's generation
/// under the class-manager writer BEFORE `JitCache::bump_redefine_epoch`
/// (`note_class_redefinition_of`). The epoch is read BEFORE the verdict, so an
/// entry stamped `e` saw every redefinition `e` counts. A stale `true` (the
/// name now resolves to a newly defined class) only keeps the site on the
/// quiesced path, which is conservative. Kill switch:
/// `CRATONVM_JIT_QUIESCE_VERDICT_MEMO=0`.
pub fn site_class_was_redefined(vm: &SharedVm, info: &JitInvokeInfo, info_key: JitSiteKey) -> bool {
    if !crate::classloading::any_class_redefined() {
        return false;
    }
    if !quiesce_verdict_memo_enabled() {
        return class_id_or_name_was_redefined(vm, info.declaring_class_id, info.class_name);
    }
    let epoch = vm.jit.jit_cache.redefine_epoch();
    let memo = QUIESCE_VERDICT.with(|m| m.borrow().get(&info_key).copied());
    if let Some((stamp, verdict)) = memo {
        if stamp == epoch {
            return verdict;
        }
    }
    let verdict = class_id_or_name_was_redefined(vm, info.declaring_class_id, info.class_name);
    QUIESCE_VERDICT.with(|m| {
        m.borrow_mut().insert(info_key, (epoch, verdict));
    });
    verdict
}

/// `CRATONVM_JIT_QUIESCE_VERDICT_MEMO`: default ON; `0` / `false` / `off`
/// asks [`class_id_or_name_was_redefined`] on every dispatch, as before.
pub fn quiesce_verdict_memo_enabled() -> bool {
    pub static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// Does `jit_invoke_dispatch_body` stand its compiled-entry arms down for a
/// redefinition of the site's class on THIS call? Round 12 wave 2, lane rt
/// (proposal R12W1-1 of `docs/feature-designs/jit-ir-optimizations.md`).
///
/// This was [`site_class_was_redefined`], which never goes back to `false`:
/// after one `Mockito.mock(Foo.class)` or one agent retransform, every
/// compiled call site into `Foo` ran its callee interpreted for the life of
/// the process. Now the site stands down once per thread per redefinition of
/// its VM: on the call that runs the quiesced-site flush of the thread's
/// raw-entry caches (`QUIESCED_SITE_FLUSH_STAMP`, stamped by the caller). The
/// MIC helper has quiesced this way, once per slot per redefinition epoch,
/// since `hierarchy_was_redefined` left it.
///
/// Sound because staleness is handled where the bodies live, not here. A
/// reader that sees the class's new generation took the class-manager read
/// lock after the redefinition's writer released it, and that writer had
/// already withdrawn every stale body (`note_class_redefinition_of`: full
/// flush, or the scoped withdrawal plus not-entrant patching). A compile that
/// began before is refused at publication (`flush_barrier`, the per-class
/// barrier). A thread-local entry naming a withdrawn body is dropped at its
/// hit (`DispatchCache::names_a_stale_body`) and by the retire generation
/// (`flush_raw_entry_dispatch_caches`). The bytecode templates carry their
/// own `RedefineGate`. A body entered after that was compiled from the
/// current bytecode, the question the per-body stamp of the proposal would
/// ask. Obsolete frames (JEP 109) are interpreter frames and never reach
/// this helper.
///
/// Kill switch `CRATONVM_JIT_QUIESCE_BY_GENERATION=0`: the permanent
/// quiescence, as before.
pub fn site_quiesced_for_redefinition(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
) -> bool {
    if !site_class_was_redefined(vm, info, info_key) {
        return false;
    }
    if !quiesce_by_generation_enabled() {
        return true;
    }
    let stamp = (vm.vm_identity, vm.jit.jit_cache.redefine_epoch());
    QUIESCED_SITE_FLUSH_STAMP.with(|seen| seen.get()) != stamp
}

/// Must [`try_resume_trapped_callee`] refuse the frame a compiled body of
/// `key_class.key_method key_desc` stashed at the point `point_addr`, because
/// that class was redefined? Round 12 wave 2, lane rt: the other half of
/// R12W1-1.
///
/// The frame is rebuilt against the method's CURRENT bytecode, so it may be
/// resumed only if the body that trapped was compiled from it. This asked
/// "was the class ever redefined", which refused every trap of every body of
/// a mocked class forever. A refused trap is re-run from the callee's entry
/// (`rerun_declined_callee_from_entry`), which replays what the compiled body
/// had already committed. The MIC helper enters such bodies after one
/// quiesced call per redefinition, and since [`site_quiesced_for_redefinition`]
/// `jit_invoke_dispatch_body` does too, so the replay was reachable on every
/// uncommon trap in them.
///
/// Now a body is current when it is the one published for the method, it owns
/// the point that fired (`CompiledMethod::owns_deopt_point`), and its
/// compilation began after the class's last redefinition
/// (`JitCache::compiled_since_redefinition_of`, the question
/// `granted_exit_of_a_current_body` asks). Anything else (a superseded or
/// withdrawn body, an OSR body, a name another loader resolves, no point)
/// keeps the refusal. The trapping body is pinned by the call arm that
/// entered it, so its points cannot be reused by the published body's.
/// `CRATONVM_JIT_QUIESCE_BY_GENERATION=0` restores the plain refusal.
///
/// The class is the one the call site's loader resolves `key_class` to
/// (`declaring_class_id`, as [`try_resume_trapped_callee`] resolves the method
/// it rebuilds the frame from, and [`stash_is_from_a_current_body`] the body):
/// interpreter round i1 wave 29, lane L3. It was the loader-blind
/// `get_loaded_class_id` name lookup, which answers the BUILT-IN loaders' class
/// of that name first, so with the same name defined by the application loader
/// (never redefined) and by a user loader (redefined: a mocked class in a
/// test framework's isolated loader), a stale body of the user loader's class
/// was judged "not redefined" and its trap rebuilt against the class's NEW
/// bytecode. A name the site's loader cannot resolve keeps the name answer.
pub fn stashed_callee_predates_redefinition(
    vm: &SharedVm,
    declaring_class_id: u32,
    key_class: &str,
    key_method: &str,
    key_desc: &str,
    point_addr: usize,
) -> bool {
    if !crate::classloading::any_class_redefined() {
        return false;
    }
    let resolved = {
        let cm = vm.classes.class_manager.read();
        if declaring_class_id == 0 {
            cm.find_bootstrap_class_by_name(key_class)
        } else {
            cm.find_cp_class_for_class(key_class, ClassId::new(declaring_class_id))
        }
    };
    let Some(class_id) = resolved else {
        // The resume below cannot resolve it either and refuses; keep the
        // answer this gave before.
        return named_class_was_redefined(vm, key_class);
    };
    if !class_was_redefined(vm, class_id) {
        return false;
    }
    if !quiesce_by_generation_enabled() || point_addr == 0 {
        return true;
    }
    let cache = &vm.jit.jit_cache;
    let by_name = cache
        .get(key_class, key_method, key_desc, class_id)
        .is_some_and(|body| {
            body.owns_deopt_point(point_addr)
                && cache.compiled_since_redefinition_of(class_id, body.install_epoch)
        });
    // Interpreter round i2 wave 61, lane L2: the body the trampoline named
    // answers first; the published body found by name only when it is absent.
    match named_trapping_body_is_current(vm, class_id, point_addr, by_name, "predates-redefinition")
    {
        Some(current) => !current,
        None => !by_name,
    }
}

/// Did the body the deopt stash names for its top entry (the trampoline's
/// own answer, [`cratonvm_jit::deopt::peek_last_deopt_trapping_body`]) run
/// `class_id`'s CURRENT bytecode -- compiled after that class's last
/// redefinition? `None` when the stash names no body of `class_id` at
/// `point_addr` (the switch off, a restash, the re-run sentinel, a body
/// published under another class's key): the caller then asks the method's
/// PUBLISHED body by name, as before.
///
/// Interpreter round i2 wave 61, lane L2: the remaining sinks of
/// `docs/feature-designs/jit-ir-optimizations.md`.
/// The name lookup answers what is published NOW, so a body superseded by a
/// tier-up (or withdrawn) after its compile -- exactly the body whose trap is
/// in flight -- was judged stale even when it had been compiled from the
/// current bytecode, and its frame went to the own-source resume or the
/// re-run from entry. `by_name` is the lookup's answer, printed beside the
/// named one under `CRATONVM_DBG_DEOPT` (`agrees=`) so the host can see where
/// the two differ. Cold: only on a callee trap of a redefined class.
pub fn named_trapping_body_is_current(
    vm: &SharedVm,
    class_id: ClassId,
    point_addr: usize,
    by_name: bool,
    sink: &str,
) -> Option<bool> {
    if point_addr == 0 || !cratonvm_jit::deopt::stash_names_trapping_body_enabled() {
        return None;
    }
    let body = cratonvm_jit::deopt::peek_last_deopt_trapping_body().filter(|body| {
        body.owns_deopt_point(point_addr) && body.owner_class_id == class_id.as_u32()
    })?;
    let current = vm
        .jit
        .jit_cache
        .compiled_since_redefinition_of(class_id, body.install_epoch);
    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] callee trap body named by the stash ({sink}): class_id={} \
             current={current} by-name={by_name} agrees={}",
            class_id.as_u32(),
            current == by_name
        );
    }
    Some(current)
}

/// `CRATONVM_JIT_QUIESCE_BY_GENERATION`: default ON; `0` / `false` / `off`
/// keeps every call through a redefined class's site quiesced for the life of
/// the process ([`site_quiesced_for_redefinition`]).
pub fn quiesce_by_generation_enabled() -> bool {
    pub static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

thread_local! {
    /// [`class_was_redefined`]'s memo: `(vm_identity, that VM's redefine
    /// epoch, its class-store definition epoch)` the verdicts were read
    /// under, and `class id -> ever redefined`.
    pub static REDEFINED_CLASS_MEMO: std::cell::RefCell<(
        (usize, u32, u64),
        rustc_hash::FxHashMap<u32, bool>,
    )> = std::cell::RefCell::new(((0, u32::MAX, 0), rustc_hash::FxHashMap::default()));
}

/// Clear-on-full bound for [`REDEFINED_CLASS_MEMO`].
pub const REDEFINED_CLASS_MEMO_CAP: usize = 4096;
