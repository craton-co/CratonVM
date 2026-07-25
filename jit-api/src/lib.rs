// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT compiler API types for CratonVM.
//!
//! Shared types used by the JIT compiler crate and the VM crate:
//! - [`CachedBytecodeMethod`] — method data needed for JIT compilation
//! - [`JitRuntimeHelpers`] — function pointer table for JIT runtime callbacks
//! - [`gpu_lowering::GpuLowering`] (under the `gpu-lowering` feature) —
//!   trait seam for emitting PTX from a resolved Java method.
//!
//! ## `gpu-lowering` feature status
//!
//! The `gpu_lowering` module is an off-by-default extension point, compiled
//! only with `--features gpu-lowering`. Current workspace GPU lowering uses
//! `cratonvm-jit-cuda`'s concrete bytecode-to-PTX entry points directly; no
//! workspace crate currently enables this feature or implements
//! [`gpu_lowering::GpuLowering`].
//!
//! The trait remains in-tree as a narrow public seam for a future pluggable PTX
//! backend. If that integration is abandoned, remove the feature, this module,
//! and the related public GPU documentation together.

#[cfg(feature = "gpu-lowering")]
pub mod gpu_lowering;

use std::sync::Arc;

use cratonvm_reader::attribute::ExceptionTableEntry;
use cratonvm_types::ClassId;

/// Cached bytecode method info — everything needed to create a Frame without
/// any lock acquisitions or string allocations.
///
/// `Clone` is implemented by hand rather than derived because
/// [`Self::jit_probe_generation`] is an `AtomicU64` (not `Clone`). The manual
/// impl snapshots its current value, which is semantically right: the field is
/// a *memo*, and copying a memo forward is always sound (at worst the clone
/// re-probes once).
pub struct CachedBytecodeMethod {
    pub declaring_class_id: ClassId,
    pub class_name: Arc<str>,
    pub method_name: Arc<str>,
    pub method_descriptor: Arc<str>,
    pub source_file: Option<Arc<str>>,
    pub code: Arc<[u8]>,
    pub exception_table: Arc<[ExceptionTableEntry]>,
    pub max_stack: u16,
    pub max_locals: u16,
    pub num_params: u16,
    pub is_synchronized: bool,
    pub is_static: bool,
    /// Perf (2026-07-15, `ClientHttpConnectorTests` interpreter-throughput
    /// investigation): memoizes the pure/deterministic part of
    /// `force_native_over_real_jdk_bytecode(class_name, method_name,
    /// method_descriptor)` (a ~1400-line sequential string-comparison
    /// special-case dispatcher in `vm/src/runtime/interpreter.rs`, already
    /// documented as consuming ~51% of all executed instructions on
    /// method-call-heavy workloads -- see
    /// `docs/known-issues/tomcat-08-07/silent-hang-no-signature-cluster.md`).
    /// This entry is `Arc`-shared across every cache hit for its callsite,
    /// so populating it once here and reading it thereafter turns an
    /// O(~55 string comparisons) recheck on every cached
    /// `invokevirtual`/`invokestatic` dispatch into an O(1) read after the
    /// first hit. The redefine-dependent wrapper around this pure check
    /// (`should_force_registered_native_over_bytecode`) is NOT cached here
    /// -- it depends on mutable per-class redefine state that can change
    /// after this entry is populated, so it is still re-evaluated on every
    /// hit (cheap: a single generation-counter read plus a short allowlist).
    pub force_native_cache: std::sync::OnceLock<bool>,
    /// Perf follow-up (2026-07-19, TestResponsePerformance interpreter-
    /// throughput residual): memoizes the resolved `NativeCallback` (or
    /// `None`) for this callsite, mirroring `force_native_cache` above.
    /// `intercept_force_registered_native_cached` previously called
    /// `NativeMethodRegistry::find` (a hash-keyed lookup over all three of
    /// class/method/descriptor) on every single cached-invoke hit whose
    /// force-native decision was `true` -- confirmed via `perf` to be the
    /// #2 hottest symbol (~7% of samples) on this same benchmark, second
    /// only to the interpreter's own frame-dispatch loop. Native-method
    /// registration is immutable after VM boot (no redefinition path
    /// touches the registry), so the same (class, method, descriptor)
    /// triple always resolves to the same callback for the life of the
    /// process -- caching it here is sound by the same argument already
    /// used for `force_native_cache`. See docs/known-issues/tomcat-08-07/
    /// silent-hang-no-signature-cluster.md.
    pub native_callback_cache: std::sync::OnceLock<Option<cratonvm_native_api::NativeCallback>>,
    /// T2.5 — memoized JIT invocation-counter key for this method, i.e. the
    /// packed `(declaring_class_id << 32) | hash(method_name ++ descriptor)`
    /// u64 that the interpreter uses to index
    /// `ProfileStore::increment_invocation`.
    ///
    /// The interpreter's `Bytecode` / `VirtualBytecode` dispatch arms recomputed
    /// this key on EVERY interpreted invocation of a not-yet-compiled method by
    /// running a 31-multiplier byte loop over both the method name and the full
    /// descriptor — pure per-call overhead on the hottest interpreter path, for
    /// a value that is a pure function of three immutable fields of this entry.
    /// Memoized here for exactly the same reason (and by exactly the same
    /// argument) as [`Self::force_native_cache`] and
    /// [`Self::native_callback_cache`] above. Read via [`Self::invoc_key`].
    pub invoc_key: std::sync::OnceLock<u64>,
    /// T2.2 — epoch memo for "this method has no published JIT body".
    ///
    /// Holds the value of `cratonvm_jit::jit_cache_generation()` as of the last
    /// time an interpreter dispatch arm probed the shared JIT cache for this
    /// method and found *nothing*. `0` means "never probed" (the live generation
    /// starts at 1 and only ever increases, so `0` can never compare equal to
    /// it).
    ///
    /// Why this exists: the interpreter's cached-invoke `Bytecode` arms had to
    /// call `JitCache::get(class, method, descriptor, class_id)` on every single
    /// interpreted call, purely to notice that a background compile had
    /// published a body for this method. That lookup hashes all three strings
    /// and then re-compares all three with full string equality — the exact
    /// re-resolution the per-call-site inline cache exists to avoid. Because
    /// *every* JIT-cache publication and invalidation bumps the global
    /// generation, comparing this snapshot against it is an equivalent test:
    /// equal ⇒ the cache content has not changed since we last looked and found
    /// nothing, so looking again cannot find anything. Steady state therefore
    /// costs two integer loads and a compare instead of three string hashes and
    /// three string comparisons.
    ///
    /// Correctness rests on the generation being bumped by every writer of the
    /// JIT cache; see `JitCache::put` / `put_osr` / `invalidate_matching` /
    /// `clear_all` in `jit/src/lib.rs`, which are the only mutators.
    pub jit_probe_generation: std::sync::atomic::AtomicU64,
}

impl Clone for CachedBytecodeMethod {
    fn clone(&self) -> Self {
        Self {
            declaring_class_id: self.declaring_class_id,
            class_name: Arc::clone(&self.class_name),
            method_name: Arc::clone(&self.method_name),
            method_descriptor: Arc::clone(&self.method_descriptor),
            source_file: self.source_file.clone(),
            code: Arc::clone(&self.code),
            exception_table: Arc::clone(&self.exception_table),
            max_stack: self.max_stack,
            max_locals: self.max_locals,
            num_params: self.num_params,
            is_synchronized: self.is_synchronized,
            is_static: self.is_static,
            force_native_cache: self.force_native_cache.clone(),
            native_callback_cache: self.native_callback_cache.clone(),
            invoc_key: self.invoc_key.clone(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(
                self.jit_probe_generation
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
        }
    }
}

impl CachedBytecodeMethod {
    /// The memoized JIT invocation-counter key for this method — see
    /// [`Self::invoc_key`]. Computed on first use, then read straight out of the
    /// `OnceLock`.
    ///
    /// The hash must stay bit-identical to the two open-coded loops this
    /// replaced (`vm/src/runtime/interpreter.rs`, the invokestatic `Bytecode`
    /// arm and the instance tier-up path), because both keyed the *same*
    /// `ProfileStore` invocation counters: a different key would silently reset
    /// every method's warmup count.
    #[inline]
    pub fn invoc_key(&self) -> u64 {
        *self.invoc_key.get_or_init(|| {
            let mut h = 0u32;
            for &b in self.method_name.as_bytes() {
                h = h.wrapping_mul(31).wrapping_add(b as u32); // Widening: hash computation
            }
            for &b in self.method_descriptor.as_bytes() {
                h = h.wrapping_mul(31).wrapping_add(b as u32); // Widening: hash computation
            }
            // Widening: class ID to u64 for hash key
            ((self.declaring_class_id.as_u32() as u64) << 32) | (h as u64)
        })
    }

    /// T2.2 — has this method already been probed against the shared JIT cache
    /// at generation `current_generation` and found to have no compiled body?
    ///
    /// `current_generation` must come from `cratonvm_jit::jit_cache_generation()`
    /// (an `Acquire` load). A `true` answer means the caller may skip the
    /// string-keyed `JitCache::get` entirely.
    #[inline]
    pub fn jit_probe_is_current(&self, current_generation: u64) -> bool {
        self.jit_probe_generation
            .load(std::sync::atomic::Ordering::Relaxed)
            == current_generation
    }

    /// T2.2 — record that a `JitCache::get` for this method returned `None`
    /// while the cache was at `generation`.
    ///
    /// `Relaxed` is sufficient: the value is only ever used to skip a lookup
    /// that would have returned `None` anyway, and the *global* generation read
    /// that guards it is an `Acquire` load, so a reader that observes a newer
    /// generation also observes the publication that caused it.
    #[inline]
    pub fn record_jit_probe_miss(&self, generation: u64) {
        self.jit_probe_generation
            .store(generation, std::sync::atomic::Ordering::Relaxed);
    }
}

/// JEP 358 (helpful NPE) — operation-kind codes carried out-of-band from a
/// JIT-originated `NullPointerException` so the interpreter's post-JIT drain
/// can attach the *action-only* JEP-358 message ("Cannot load from int array",
/// "Cannot read the array length", …).
///
/// **Single source of truth.** These `u8` codes are baked as `MOV` immediates
/// by the inline null-check stubs in `jit/src/x64.rs` (the JIT crate, which
/// cannot depend on the VM crate) AND mapped to their HotSpot `BytecodeUtils`
/// strings by `vm/src/runtime/exceptions.rs::helpful_npe` (which re-exports this
/// module as `jit_action`). Keeping the numeric vocabulary in `jit-api` — the
/// one crate both sides already depend on — removes the drift hazard of two
/// hand-kept copies.
///
/// Codes `0..=7` are the increment-4 originals (length + int/object/byte
/// load/store); `8..=17` extend the vocabulary to the remaining primitive
/// element kinds (long/float/double/char/short) so the inline-codegen null
/// path can name every array element type HotSpot does. Values are **stable**
/// (append-only): the JIT bakes them into RWX code, so never renumber.
///
/// `baload`/`bastore` cannot distinguish `byte[]` from `boolean[]` at the null
/// site (same opcode, null receiver), so both map to [`ALOAD_BYTE`]/
/// [`ASTORE_BYTE`] — matching the existing per-type JIT helpers. (HotSpot
/// spells this "byte/boolean"; the divergence is pre-existing and tracked
/// separately, not introduced here.)
pub mod npe_action {
    /// No action recorded — the interpreter emits an unmessaged NPE
    /// (today's default shape). Inline sites with no precise array kind
    /// (e.g. an intrinsic's array null-check) use this.
    pub const NONE: u8 = 0;
    /// `arraylength` on a null array.
    pub const ARRAY_LENGTH: u8 = 1;
    /// `iaload` on a null `int[]`.
    pub const ALOAD_INT: u8 = 2;
    /// `aaload` on a null reference array.
    pub const ALOAD_OBJECT: u8 = 3;
    /// `baload` on a null `byte[]`/`boolean[]`.
    pub const ALOAD_BYTE: u8 = 4;
    /// `iastore` into a null `int[]`.
    pub const ASTORE_INT: u8 = 5;
    /// `aastore` into a null reference array.
    pub const ASTORE_OBJECT: u8 = 6;
    /// `bastore` into a null `byte[]`/`boolean[]`.
    pub const ASTORE_BYTE: u8 = 7;
    /// `laload` on a null `long[]`.
    pub const ALOAD_LONG: u8 = 8;
    /// `faload` on a null `float[]`.
    pub const ALOAD_FLOAT: u8 = 9;
    /// `daload` on a null `double[]`.
    pub const ALOAD_DOUBLE: u8 = 10;
    /// `caload` on a null `char[]`.
    pub const ALOAD_CHAR: u8 = 11;
    /// `saload` on a null `short[]`.
    pub const ALOAD_SHORT: u8 = 12;
    /// `lastore` into a null `long[]`.
    pub const ASTORE_LONG: u8 = 13;
    /// `fastore` into a null `float[]`.
    pub const ASTORE_FLOAT: u8 = 14;
    /// `dastore` into a null `double[]`.
    pub const ASTORE_DOUBLE: u8 = 15;
    /// `castore` into a null `char[]`.
    pub const ASTORE_CHAR: u8 = 16;
    /// `sastore` into a null `short[]`.
    pub const ASTORE_SHORT: u8 = 17;
}

/// Function pointer table for JIT runtime callbacks.
///
/// The JIT compiler embeds these addresses into generated machine code as
/// absolute `CALL` targets. Each pointer is the address of an `extern "C"`
/// function implemented in the VM crate.
///
/// `#[repr(C)]` is mandatory: this struct is an explicit ABI boundary. The
/// JIT may read a field either by name or by computed byte offset; the
/// default `repr(Rust)` layout is unspecified and the compiler is free to
/// reorder fields, so a stable C layout is the only sound contract here.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct JitRuntimeHelpers {
    pub newarray: usize,
    pub new_object: usize,
    pub anewarray_object: usize,
    pub baload: usize,
    pub bastore: usize,
    pub iaload: usize,
    pub iastore: usize,
    pub aaload: usize,
    pub aastore: usize,
    pub multianewarray_2d: usize,
    pub arraylength: usize,
    pub getfield: usize,
    pub putfield_int: usize,
    pub putfield_long: usize,
    pub putfield_float: usize,
    pub putfield_double: usize,
    pub putfield_object: usize,
    pub getstatic: usize,
    pub putstatic_int: usize,
    pub putstatic_long: usize,
    pub putstatic_float: usize,
    pub putstatic_double: usize,
    pub putstatic_object: usize,
    pub checkcast: usize,
    pub instanceof_check: usize,
    pub throw_aioobe: usize,
    /// Direct-throw helper for `ArithmeticException` ("/ by zero"), the
    /// div-by-zero sibling of [`throw_aioobe`]. Called from the `idiv`/`irem`/
    /// `ldiv`/`lrem` zero-divisor guard stub: sets a pending-arithmetic flag and
    /// returns the `i64::MIN` deopt sentinel so the interpreter throws the
    /// exception through the method's exception table WITHOUT re-running the
    /// method from entry. The prior path (`uncommon_trap` → re-run) double-
    /// executed any side effect preceding the trap (HotSpot does not).
    /// Signature: `extern "C" fn() -> i64`.
    pub throw_arithmetic: usize,
    pub invoke_dispatch: usize,
    pub invoke_virtual_mic: usize,
    /// Scalar lambda adapter for TDigest numeric kernels.
    pub lambda_int_to_double: usize,
    pub write_barrier: usize,
    /// Round-7 fix (CRIT, UAF in JIT): SATB pre-write barrier.
    ///
    /// Logs the *old* reference value before a reference store, so the
    /// concurrent marker maintains the snapshot-at-the-beginning invariant.
    /// Without this, JIT-compiled `aastore`/`putfield`/`putstatic`
    /// overwriting a still-live reference during concurrent marking would
    /// drop the only path to the overwritten target → missed mark →
    /// use-after-free on the next mixed evacuation.
    ///
    /// Signature: `extern "C" fn(vm_ptr: i64, old_ref: i64)`.
    /// The helper short-circuits cheaply (single Acquire load) when
    /// `SatbQueue::is_active() == false`, which is the steady-state when
    /// no concurrent mark cycle is in flight.
    pub satb_pre_write_barrier: usize,
    /// Uncommon trap handler: called from JIT code when a speculative
    /// optimization fails (wrong receiver type, unreached branch, etc.).
    /// Signature: extern "C" fn(vm_ptr: i64, reason: i64, bci: i64) -> i64
    /// Returns a deopt action code (0=reinterpret, 1=recompile, 2=blacklist).
    pub uncommon_trap: usize,
    /// T1.1.28 — `Math.fma(double, double, double)` fused multiply-add.
    /// Signature: `extern "C" fn(a: f64, b: f64, c: f64) -> f64`.
    /// Implemented via Rust's `f64::mul_add`, which emits `VFMADD231SD`
    /// when the target CPU supports FMA3 and otherwise performs a
    /// software-correct single-rounding fused operation.
    pub math_fma_double: usize,
    /// T1.1.28 — `Math.fma(float, float, float)`.
    /// Signature: `extern "C" fn(a: f32, b: f32, c: f32) -> f32`.
    pub math_fma_float: usize,

    // -----------------------------------------------------------------
    // Inline TLAB bump-pointer wiring (HIGH-6 JIT audit, object_allocation)
    //
    // These three `usize`s are NOT function pointers — they are byte
    // offsets the JIT bakes as immediates into the inline TLAB
    // fast-path emitted by `jit/src/x64.rs::new` (opcode 0xbb).
    //
    // The helper-table populator (`vm/src/jit/helpers.rs::build_helpers`)
    // computes them once at startup from `JvmThread::tlab_offset()` plus
    // `Tlab::CURSOR_OFFSET` / `Tlab::END_OFFSET`. The runtime asserts in
    // `Tlab::test_tlab_offsets` and `JvmThread::tlab_offset_matches_field_address`
    // guard the layout — change them in lockstep.
    // -----------------------------------------------------------------
    /// Byte offset of `Tlab::cursor` from `&JvmThread` (i.e. the full
    /// `JvmThread → tlab → cursor` chain). Emitted as `[thread+disp32]`
    /// in the inline bump fast path.
    pub tlab_cursor_offset_in_thread: usize,
    /// Byte offset of `Tlab::end` from `&JvmThread`.
    pub tlab_end_offset_in_thread: usize,
    /// Byte offset of `ObjectHeader.class_id` from the object base.
    /// Currently `0` (JIT contract enforced by `class_id_remains_at_offset_zero`
    /// in `types/src/heap_types.rs`), exposed here so the JIT does not
    /// hardcode the constant in a second place.
    pub class_id_offset_in_obj: usize,
    /// Address of the small `extern "C" fn() -> *mut JvmThread` helper
    /// that returns the current thread's `JvmThread` pointer via the
    /// `JIT_THREAD` thread-local. Used by the inline TLAB bump as the
    /// first step (one CALL is cheaper than a full `jit_new_object`
    /// dispatch). Set to `0` when not wired (JIT then falls back to the
    /// helper-call path).
    pub get_current_thread: usize,
    /// Address of `extern "C" fn(vm_ptr, obj_ptr, class_id, num_fields)
    /// -> i64`, the inline-TLAB completion helper. Called by the JIT
    /// after a successful inline bump-pointer to finish the object
    /// header (kind/hash/num_slots), apply primitive-typed defaults
    /// from class metadata, and register finalizable classes. Returns
    /// the object pointer unchanged.
    pub tlab_post_init: usize,
    /// Stage 3 (precise oop maps) — address of
    /// `extern "C" fn(rbp: usize)`, called once in the JIT prologue when
    /// `CRATONVM_PRECISE_JIT_MAPS` is on to register this frame's EXACT
    /// RBP with the GC root walker (the Rust-side `JitEntryGuard` only
    /// captures an approximate SP). `0` = not wired → the prologue skips
    /// the call and the walker uses the conservative path. Optional.
    pub frame_record: usize,
    /// Shadow-stack precise roots (`CRATONVM_SHADOW_STACK`) — byte offset of
    /// the `ShadowStack` field from `&JvmThread`. The JIT emits the inline
    /// push as `[thread + shadow_stack_offset_in_thread + ShadowStack::TOP_OFFSET]`.
    /// `0` when the mechanism is off → no shadow codegen is emitted.
    pub shadow_stack_offset_in_thread: usize,
    /// RBC.6 (athrow codegen) — `extern "C" fn(exc_ptr: i64) -> i64`.
    /// Stashes the thrown exception object as the pending JIT exception
    /// (or sets the pending-NPE flag when `exc_ptr == 0`, per JVMS athrow-
    /// on-null semantics) and returns the `i64::MIN` deopt sentinel. The
    /// 0xbf codegen arm calls this and immediately runs the method
    /// epilogue; the interpreter's JIT-return drains surface the
    /// exception. Appended at the END of the struct so all prior golden
    /// offsets stay stable.
    pub throw_exception: usize,
    /// JEP 358 (helpful NPE), inline-codegen path — `extern "C" fn(code: i64)`.
    /// Called by the per-action inline null-check failure stubs emitted in
    /// `jit/src/x64.rs::emit_null_check_store_stubs`. Sets the pending-NPE flag
    /// *with* the JEP-358 [`npe_action`] code passed in `code` (so the
    /// interpreter's post-JIT NPE drain attaches the right action-only message
    /// — "Cannot load from int array", …, gated behind
    /// `-XX:+ShowCodeDetailsInExceptionMessages`) AND the out-of-band deopt
    /// signal (each stub loads `i64::MIN` as the method return value). This
    /// replaces the prior single shared stub that called `bastore(0)` and
    /// therefore fabricated the byte-store action for every array opcode.
    /// Appended at the END of the struct so all prior golden offsets stay
    /// stable.
    pub jit_npe_with_action: usize,
    /// `i64::MIN`-sentinel disambiguation for `J`/`D` (long/double) call returns
    /// — `extern "C" fn() -> i64`.
    ///
    /// The dispatch helpers (`invoke_dispatch` / `invoke_virtual_mic`) signal a
    /// callee exception/deopt to a compiled caller by returning the `i64::MIN`
    /// sentinel in RAX. For an `int`/ref/void return that is unambiguous
    /// (`i64::MIN` is not a valid sign-extended `int`, oop, or void result), so
    /// the post-invoke check is a plain `CMP RAX, i64::MIN; JE bail`. But a
    /// callee that *legitimately* returns `Long.MIN_VALUE` (a `J`/`D` whose bits
    /// equal `i64::MIN`) returns the SAME value WITHOUT any pending-signal flag —
    /// so a `J`/`D` call site cannot tell the two apart from RAX alone.
    ///
    /// At a `J`/`D` call site the backend therefore emits, only on the (rare)
    /// `RAX == i64::MIN` branch, a `CALL` to this helper, which PEEKS (does not
    /// clear) every out-of-band signal — pending exception / NPE / AIOOBE /
    /// deopt flag, and a stashed IR-deopt frame — and returns `1` iff a genuine
    /// exception/deopt is pending (caller bails), `0` iff the `i64::MIN` is a
    /// real return value (caller keeps it). The common (`RAX != i64::MIN`) path
    /// is untouched, so int/ref/void call sites stay byte-identical.
    /// Appended at the END of the struct so all prior golden offsets stay
    /// stable.
    pub dispatch_threw: usize,
    /// IR FP tier (Slice A) — `frem` runtime helper, `extern "C" fn(f32, f32)
    /// -> f32`. JVM `frem` is the `fmod`-style truncated remainder (sign of the
    /// dividend, with the matching NaN/∞ rules) which has no single SSE
    /// instruction; the IR `Op::Rem` Float arm loads the two operands into
    /// XMM0/XMM1 and `CALL`s this helper (result in XMM0). The float ABI passes
    /// the two args in XMM0/XMM1 and returns in XMM0 on both Win64 and SysV,
    /// matching the IR's own XMM scratch convention exactly, so no register
    /// shuffling is needed. Appended at the END of the struct so all prior
    /// golden offsets stay stable.
    pub jit_frem: usize,
    /// IR FP tier (Slice A) — `drem` runtime helper, `extern "C" fn(f64, f64)
    /// -> f64`. The double analogue of [`Self::jit_frem`]; the IR `Op::Rem`
    /// Double arm `CALL`s it with the operands in XMM0/XMM1 and reads the
    /// remainder from XMM0. Appended at the END of the struct so all prior
    /// golden offsets stay stable.
    pub jit_drem: usize,
    /// Native-stack headroom guard for direct self-recursive calls —
    /// `extern "C" fn(vm_ptr: i64) -> i64`.
    ///
    /// Historically every non-tail self-recursive `invokestatic` was routed
    /// through `invoke_dispatch` solely so its thread-local depth guard could
    /// convert runaway compiled recursion into a catchable
    /// `StackOverflowError` (BUG-1). That made every recursive call pay the
    /// full dispatch-helper round trip (TLS scan-cache invalidation, SATB
    /// flush, dispatch-cache lookup, …) — ~10-30x the cost of the `CALL`
    /// itself, the dominant term in recursive workloads (fib, binarytrees).
    ///
    /// With this helper wired, the backend instead emits a direct rel32
    /// self-`CALL` preceded by one `CALL` to this guard. The guard compares
    /// the current native stack pointer against a per-thread floor (queried
    /// from the OS once per thread and cached): comfortably above the floor
    /// it returns `0` and the site proceeds to the direct self-call; at
    /// exhaustion it stashes a catchable `java/lang/StackOverflowError`
    /// (exactly like the dispatch depth guard) and returns the `i64::MIN`
    /// deopt sentinel, which the site routes through the existing post-invoke
    /// sentinel check. `0` = not wired → the jit crate keeps routing self
    /// calls through `invoke_dispatch` (historical behaviour). Appended at
    /// the END of the struct so all prior golden offsets stay stable.
    pub self_call_stack_guard: usize,
    /// Guarded inline `getfield` — address of the GC's process-global
    /// `JIT_REGION_BOUNDS` table (`[yf_base, yf_end, yt_base, yt_end,
    /// og_base, og_end]`, six consecutive `AtomicUsize` words), NOT a
    /// function pointer.
    ///
    /// When non-zero, the single-pass backend's default `getfield` arm emits
    /// an inline receiver guard — null/alignment bit-tests plus the same
    /// three-region `[base, end)` containment check `jit_getfield`'s
    /// `is_object_address` gate performs — and on success reads the field
    /// cell directly (a receiver inside a published region points at arena
    /// memory that stays mapped, so the raw load cannot fault). Guard
    /// failures branch to the checked `jit_getfield` helper, preserving its
    /// NPE/sentinel semantics for null and implausible receivers. `0` = not
    /// wired (G1/ZGC backends, or before the first publish) → every getfield
    /// keeps the checked-helper path. Appended at the END of the struct so
    /// all prior golden offsets stay stable.
    pub region_bounds_addr: usize,
    /// Inline self-recursion guard — address of the leaf
    /// `jit_native_stack_floor` helper (`extern "C" fn() -> i64`), which
    /// returns the current OS thread's native-stack floor (get-or-compute of
    /// the same TLS value `self_call_stack_guard` uses; touches no VM state,
    /// never GCs).
    ///
    /// When wired, a method with direct self-recursive call sites reserves a
    /// frame slot, calls this once in its prologue, and each self-call site
    /// emits `CMP RSP, [rbp - floor_slot]; JA <skip guard>` — replacing the
    /// per-recursion-level `self_call_stack_guard` helper CALL (plus its
    /// safepoint spill / shadow push+reload bracketing) on the common path.
    /// The helper CALL remains verbatim as the fallback and still owns the
    /// catchable StackOverflowError raise. OSR trampolines initialise the
    /// slot to `usize::MAX` (`RSP > MAX` is unsatisfiable), so OSR-entered
    /// frames always take the helper. `0` = not wired → sites keep the
    /// unconditional helper CALL. Appended at the END of the struct so all
    /// prior golden offsets stay stable.
    pub native_stack_floor_fn: usize,
    /// Materializes an interned Java String for a compiled `ldc` site.
    /// Signature: `extern "C" fn(vm_ptr, utf8_ptr, utf8_len) -> i64`.
    /// The helper performs the string-pool lookup on every execution so the
    /// returned reference remains valid after a relocating collection; JIT code
    /// must never bake a managed-object address as an immediate.
    pub ldc_string: usize,
    /// Cooperative JIT safepoint polling (`CRATONVM_JIT_SAFEPOINT_POLLS`, off
    /// by default) — address of the single STW-requested flag byte
    /// (`GcBarrier::stw_requested`, see
    /// `vm/src/threading/gc_barrier.rs::stw_requested_flag_addr`), NOT a
    /// function pointer. The JIT backend bakes this address as an absolute
    /// immediate (`MOV R11, imm64`) and emits `TEST byte ptr [R11], 0xFF; JNZ
    /// slow` at method entry (context methods only) and on every `goto`
    /// loop back-edge when the flag is wired. `0` = polling disabled
    /// entirely — the backend emits no poll code and GC continues to rely
    /// solely on the `SuspendThread`-based conservative scan
    /// (`vm/src/jit/xt_root_scan.rs`) for threads running JIT code. The
    /// `GcBarrier` this points at lives inside `Arc<SharedVm>`, so the
    /// address is stable for the life of the VM (see the stability contract
    /// on `stw_requested_flag_addr`). Appended at the END of the struct so
    /// all prior golden offsets stay stable.
    pub safepoint_flag_addr: usize,
    /// Cooperative JIT safepoint polling slow path — address of
    /// `extern "C" fn(vm_ptr: i64)`
    /// (`vm/src/jit/helpers.rs::jit_safepoint_slow_path`). Called only on a
    /// poll hit (the inline flag-byte check observed a nonzero value):
    /// the poll site first performs the existing pre-safepoint register
    /// spill (`x64.rs::emit_pre_safepoint_spill`) so the frame's oop map is
    /// valid, THEN calls this helper, which joins the same stop-the-world
    /// wait the interpreter's own poll hit uses
    /// (`vm/src/runtime/interpreter.rs::safepoint_check`) so GC observes
    /// this thread's roots exactly like an interpreter frame.
    /// `vm/src/jit/helpers.rs::build_helpers` wires this unconditionally
    /// (the function always exists); [`Self::safepoint_flag_addr`] is what
    /// actually gates whether the JIT ever emits a `CALL` to it, so this
    /// field being non-zero while that one is `0` is harmless (dead code,
    /// never reached). Appended at the END of the struct so all prior
    /// golden offsets stay stable.
    pub safepoint_slow_path: usize,
    /// Stable address of the generational collector's atomic card-byte array.
    ///
    /// Together with `jit_card_old_base/end`, this enables the x64 backend to
    /// emit the post-store card mark inline. All three fields are zero for G1
    /// and ZGC, whose remembered-set protocols remain helper-owned.
    pub jit_card_table_addr: usize,
    /// Inclusive old-generation base covered by `jit_card_table_addr`.
    pub jit_card_old_base: usize,
    /// Exclusive old-generation end covered by `jit_card_table_addr`.
    pub jit_card_old_end: usize,
}

/// Classifies each field of [`JitRuntimeHelpers`] for the validator.
///
/// Drives the `helper_fields!` macro below — the macro emits one
/// `FieldEntry` per struct field, and [`JitRuntimeHelpers::validate`]
/// branches on `kind` so the right contract is applied per slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    /// Mandatory `CALL` target. Zero (null) means a baked absolute call
    /// would fault — `validate()` rejects.
    RequiredPtr,
    /// Optional `CALL` target. Zero means "not wired, JIT falls back to
    /// the helper-call slow path"; non-zero must be a real entry point.
    /// `validate()` accepts both — non-zero is automatically non-null.
    OptionalPtr,
    /// Byte offset (immediate), not a pointer. Zero is a legitimate value
    /// (e.g. `class_id_offset_in_obj` is 0 by contract). `validate()`
    /// ignores it.
    Offset,
}

/// One macro-generated entry per field of [`JitRuntimeHelpers`].
///
/// Carrying `name`, `kind`, and `value` together lets [`JitRuntimeHelpers::null_pointers`]
/// and [`JitRuntimeHelpers::validate`] iterate the same single source of
/// truth — there is no longer a parallel `[usize; N]` / `[&str; N]` pair
/// that can drift.
#[derive(Clone, Copy, Debug)]
pub struct FieldEntry {
    pub name: &'static str,
    pub kind: FieldKind,
    pub value: usize,
}

// ---------------------------------------------------------------------
// Single source of truth for the `JitRuntimeHelpers` field list.
//
// `helper_fields!` expands to:
//   * `JitRuntimeHelpers::all_fields(&self) -> [FieldEntry; NUM_FIELDS]`
//   * `JitRuntimeHelpers::NUM_FIELDS: usize`
//
// To add a new field: add it to the `helper_fields!` invocation below
// and add the matching `pub <name>: usize,` to the struct. NUM_FIELDS
// is computed from the macro input — no separate constant to bump.
//
// `repr(C)` + every field being `usize` means the layout is sequential,
// so the golden-offset test in `mod tests` can validate the on-disk
// shape against documented byte offsets.
// ---------------------------------------------------------------------
macro_rules! helper_fields {
    ( $( ($name:ident, $kind:expr) ),* $(,)? ) => {
        impl JitRuntimeHelpers {
            /// Number of fields in the bulk-validation array.
            ///
            /// Computed by the `helper_fields!` macro from the field list
            /// — there is no separate hand-maintained constant to bump
            /// when a field is added. The `const _: () = assert!(...)`
            /// below additionally ties this to
            /// `size_of::<JitRuntimeHelpers>() / size_of::<usize>()`, so
            /// adding a field to the *struct* without also adding it to
            /// the macro invocation is a compile-time error.
            pub const NUM_FIELDS: usize = [ $( helper_fields!(@unit $name) ),* ].len();

            /// All fields, tagged with classification, for bulk validation.
            ///
            /// Macro-generated from the `helper_fields!` invocation so the
            /// field list lives in exactly one place. Length is pinned by
            /// the return type, kind is per-entry, and `null_pointers`
            /// and `validate` both consume this array — the previous
            /// drift hazard between `all_pointers` and `field_names` is
            /// gone.
            fn all_fields(&self) -> [FieldEntry; Self::NUM_FIELDS] {
                [
                    $(
                        FieldEntry {
                            name: stringify!($name),
                            kind: $kind,
                            value: self.$name,
                        },
                    )*
                ]
            }
        }
    };
    (@unit $name:ident) => { () };
}

helper_fields! {
    (newarray,                       FieldKind::RequiredPtr),
    (new_object,                     FieldKind::RequiredPtr),
    (anewarray_object,               FieldKind::RequiredPtr),
    (baload,                         FieldKind::RequiredPtr),
    (bastore,                        FieldKind::RequiredPtr),
    (iaload,                         FieldKind::RequiredPtr),
    (iastore,                        FieldKind::RequiredPtr),
    (aaload,                         FieldKind::RequiredPtr),
    (aastore,                        FieldKind::RequiredPtr),
    (multianewarray_2d,              FieldKind::RequiredPtr),
    (arraylength,                    FieldKind::RequiredPtr),
    (getfield,                       FieldKind::RequiredPtr),
    (putfield_int,                   FieldKind::RequiredPtr),
    (putfield_long,                  FieldKind::RequiredPtr),
    (putfield_float,                 FieldKind::RequiredPtr),
    (putfield_double,                FieldKind::RequiredPtr),
    (putfield_object,                FieldKind::RequiredPtr),
    (getstatic,                      FieldKind::RequiredPtr),
    (putstatic_int,                  FieldKind::RequiredPtr),
    (putstatic_long,                 FieldKind::RequiredPtr),
    (putstatic_float,                FieldKind::RequiredPtr),
    (putstatic_double,               FieldKind::RequiredPtr),
    (putstatic_object,               FieldKind::RequiredPtr),
    (checkcast,                      FieldKind::RequiredPtr),
    (instanceof_check,               FieldKind::RequiredPtr),
    (throw_aioobe,                   FieldKind::RequiredPtr),
    (throw_arithmetic,               FieldKind::RequiredPtr),
    (invoke_dispatch,                FieldKind::RequiredPtr),
    (invoke_virtual_mic,             FieldKind::RequiredPtr),
    (lambda_int_to_double,           FieldKind::RequiredPtr),
    (write_barrier,                  FieldKind::RequiredPtr),
    (satb_pre_write_barrier,         FieldKind::RequiredPtr),
    (uncommon_trap,                  FieldKind::RequiredPtr),
    (math_fma_double,                FieldKind::RequiredPtr),
    (math_fma_float,                 FieldKind::RequiredPtr),
    (tlab_cursor_offset_in_thread,   FieldKind::Offset),
    (tlab_end_offset_in_thread,      FieldKind::Offset),
    (class_id_offset_in_obj,         FieldKind::Offset),
    (get_current_thread,             FieldKind::OptionalPtr),
    (tlab_post_init,                 FieldKind::OptionalPtr),
    (frame_record,                   FieldKind::OptionalPtr),
    (shadow_stack_offset_in_thread,  FieldKind::Offset),
    (throw_exception,                FieldKind::RequiredPtr),
    (jit_npe_with_action,            FieldKind::RequiredPtr),
    (dispatch_threw,                 FieldKind::RequiredPtr),
    (jit_frem,                       FieldKind::RequiredPtr),
    (jit_drem,                       FieldKind::RequiredPtr),
    (self_call_stack_guard,          FieldKind::OptionalPtr),
    // NOT a pointer: address of the GC's JIT_REGION_BOUNDS table, baked as an
    // immediate by the guarded inline getfield. 0 = not wired (helper-only).
    (region_bounds_addr,             FieldKind::Offset),
    // Leaf floor-query helper for the inline self-recursion check.
    (native_stack_floor_fn,          FieldKind::OptionalPtr),
    (ldc_string,                     FieldKind::RequiredPtr),
    // Cooperative JIT safepoint polling (CRATONVM_JIT_SAFEPOINT_POLLS,
    // off by default). Address of the STW-requested flag byte, not a
    // pointer — 0 = polling disabled (matches the region_bounds_addr
    // "optional address" convention above).
    (safepoint_flag_addr,            FieldKind::Offset),
    // Slow-path helper called on a poll hit. build_helpers wires this
    // unconditionally; safepoint_flag_addr above is what actually gates
    // whether the JIT ever emits a CALL to it, so 0 here is only ever
    // "not wired" for a hand-built test helpers table.
    (safepoint_slow_path,            FieldKind::OptionalPtr),
    // Optional generational inline-card metadata. These are data addresses /
    // bounds rather than callable targets and are all zero for G1/ZGC.
    (jit_card_table_addr,             FieldKind::Offset),
    (jit_card_old_base,               FieldKind::Offset),
    (jit_card_old_end,                FieldKind::Offset),
}

// Compile-time integrity check: the macro-generated NUM_FIELDS must
// match `sizeof(JitRuntimeHelpers) / sizeof(usize)`. Every field of
// `JitRuntimeHelpers` is a `usize` and the struct is `#[repr(C)]` with
// no padding, so this ratio equals the field count. If a future
// contributor adds `pub foo: usize` to the struct without adding `foo`
// to the `helper_fields!` invocation, this assert fails — the silent
// "field omitted from validation" failure mode is closed at build time.
//
// Uses a `const _:` item rather than an inline `const { ... }` block to
// stay compatible with Rust 1.77 (inline const expressions stabilised
// in 1.79; the workspace MSRV is below that).
const _: () = assert!(
    JitRuntimeHelpers::NUM_FIELDS
        == core::mem::size_of::<JitRuntimeHelpers>() / core::mem::size_of::<usize>(),
    "JitRuntimeHelpers::NUM_FIELDS drifted from struct field count — add the new field \
     to the helper_fields! invocation in jit-api/src/lib.rs",
);

// Belt-and-suspenders: pin the exact expected count so a *removal* of a
// field also requires touching this line. Without this, deleting a
// struct field AND its macro entry simultaneously would still satisfy
// the ratio assert above and silently change the JIT ABI.
const _: () = assert!(
    JitRuntimeHelpers::NUM_FIELDS == 56,
    "JitRuntimeHelpers field count changed — bump the literal here and update \
     the golden-offset test in mod tests if the change is intentional",
);

// Compile-time enforcement of the golden ABI's per-field stride. The JIT
// targets x86-64 exclusively and the documented helper offsets are computed
// as `index * 8`, assuming an 8-byte `usize`. The `jit_runtime_helpers_field_size_is_eight`
// test validates this, but a #[test] only runs when tests are compiled and
// executed — a hypothetical 32-bit build would compile cleanly and silently
// emit wrong offsets (4-byte stride). Pinning the size here turns that into
// a build failure on any non-8-byte-usize target instead of a miscompile.
const _: () = assert!(
    core::mem::size_of::<usize>() == 8,
    "JIT golden ABI requires an 8-byte usize (x86-64); offsets are computed as \
     index * 8 and would be wrong on a non-64-bit target",
);

impl JitRuntimeHelpers {
    /// Validate that every mandatory helper pointer is non-null.
    ///
    /// A null (zero) function pointer is invalid — there is no function at
    /// address 0, so an absolute `CALL` baked from it would fault. Non-null
    /// is the *only* sound validation we can apply: function entry points
    /// have no guaranteed alignment. On x86-64 a function may legitimately
    /// start at an odd address, so a parity/alignment check would
    /// false-positive on a perfectly valid helper.
    ///
    /// Iteration is over [`Self::all_fields`], the macro-generated list,
    /// so this method automatically covers every field of the struct.
    /// Per-field semantics:
    ///
    /// * [`FieldKind::RequiredPtr`] — must be non-zero. Returns `Err`
    ///   listing the offending field names otherwise.
    /// * [`FieldKind::OptionalPtr`] — zero means "not wired" and is OK;
    ///   any non-zero value is automatically non-null (so no additional
    ///   check is needed, but the slot is still covered by the iteration
    ///   — future tightening can add e.g. address-range sanity here).
    /// * [`FieldKind::Offset`] — byte offset, not a pointer. Zero is a
    ///   legitimate value (`class_id_offset_in_obj` is 0 by contract).
    ///   Skipped.
    ///
    /// Returns `Ok(())` when every required pointer is non-null. Returns
    /// `Err(Vec<&'static str>)` with the names of the null required
    /// pointers — useful for surfacing a list of misses in a single
    /// error message rather than failing on the first one. (Round-9
    /// fix: the previous bool-returning, dead-loop implementation
    /// silently returned `true` on a null `tlab_post_init` because the
    /// hand-maintained bulk array did not include it. The validator now
    /// iterates the macro-generated `all_fields()` list — all 46 fields,
    /// 40 of which are required pointers — so no field can be silently
    /// uncovered.)
    pub fn validate(&self) -> Result<(), Vec<&'static str>> {
        let nulls = self.null_pointers();
        if nulls.is_empty() {
            Ok(())
        } else {
            Err(nulls)
        }
    }

    /// Return the names of mandatory pointer fields whose value is null.
    ///
    /// Only [`FieldKind::RequiredPtr`] fields are reported — an unset
    /// optional helper is *expected* to be zero (and the offset fields
    /// can legitimately be zero), so neither would be a bug.
    pub fn null_pointers(&self) -> Vec<&'static str> {
        self.all_fields()
            .iter()
            .filter(|e| e.kind == FieldKind::RequiredPtr && e.value == 0)
            .map(|e| e.name)
            .collect()
    }
}

// AUDIT 2026-05-16: JitRuntimeHelpersBuilder has been deleted. It was
// unused — the real construction site is `vm/src/jit/helpers.rs` (struct-
// literal init) and the only callers of the Builder were this crate's own
// tests. The stringly-typed `set(name: &str, addr: usize)` silently
// `eprintln!`-degraded on typos, providing no compile-time safety while
// duplicating the field list across five call sites. If a future
// caller wants a builder pattern, use the struct literal directly or
// generate it from a declarative macro keyed on `field_names()`.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_cached_method() -> CachedBytecodeMethod {
        CachedBytecodeMethod {
            declaring_class_id: ClassId::new(1),
            class_name: Arc::from("java/lang/Object"),
            method_name: Arc::from("hashCode"),
            method_descriptor: Arc::from("()I"),
            source_file: Some(Arc::from("Object.java")),
            code: Arc::from(vec![0xB1u8].as_slice()),
            exception_table: Arc::from(vec![].as_slice()),
            max_stack: 2,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: false,
            force_native_cache: std::sync::OnceLock::new(),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// T2.2 — the memo primitive, tested against literal generation values so
    /// it is completely independent of the process-global counter (and therefore
    /// race-free under a parallel `cargo test`). The end-to-end protocol against
    /// a real `JitCache` is tested in the `cratonvm-jit` crate.
    #[test]
    fn jit_probe_generation_memo_reports_current_only_for_the_recorded_value() {
        let cached = make_cached_method();
        // A fresh entry has never probed: 0 can never equal a live generation
        // (`JIT_CACHE_GENERATION` starts at 1 and only increases).
        assert!(!cached.jit_probe_is_current(1));
        assert!(!cached.jit_probe_is_current(u64::MAX));

        cached.record_jit_probe_miss(17);
        // Fast path engages only for the exact generation the probe ran at.
        assert!(cached.jit_probe_is_current(17));
        assert!(!cached.jit_probe_is_current(18));
        assert!(!cached.jit_probe_is_current(16));

        // A later probe at a newer generation replaces the memo.
        cached.record_jit_probe_miss(18);
        assert!(cached.jit_probe_is_current(18));
        assert!(!cached.jit_probe_is_current(17));
    }

    /// The manual `Clone` impl must carry the memos forward, not silently drop
    /// them (a dropped memo is only a perf regression, but a *wrong* one would
    /// be a correctness bug, so pin the exact values).
    #[test]
    fn clone_preserves_the_memo_fields() {
        let cached = make_cached_method();
        cached.record_jit_probe_miss(99);
        let key = cached.invoc_key();
        let _ = cached.force_native_cache.set(true);

        let copy = cached.clone();
        assert!(copy.jit_probe_is_current(99));
        assert_eq!(copy.invoc_key(), key);
        assert_eq!(copy.force_native_cache.get(), Some(&true));

        // The clone's atomic is independent of the original's.
        copy.record_jit_probe_miss(100);
        assert!(cached.jit_probe_is_current(99));
        assert!(copy.jit_probe_is_current(100));
    }

    fn make_helpers() -> JitRuntimeHelpers {
        JitRuntimeHelpers {
            newarray: 0x1000,
            new_object: 0x1008,
            anewarray_object: 0x1010,
            baload: 0x1018,
            bastore: 0x1020,
            iaload: 0x1028,
            iastore: 0x1030,
            aaload: 0x1038,
            aastore: 0x1040,
            multianewarray_2d: 0x1048,
            arraylength: 0x1050,
            getfield: 0x1058,
            putfield_int: 0x1060,
            putfield_long: 0x1068,
            putfield_float: 0x1070,
            putfield_double: 0x1078,
            putfield_object: 0x1080,
            getstatic: 0x1088,
            putstatic_int: 0x1090,
            putstatic_long: 0x1098,
            putstatic_float: 0x10A0,
            putstatic_double: 0x10A8,
            putstatic_object: 0x10B0,
            checkcast: 0x10B8,
            instanceof_check: 0x10C0,
            throw_aioobe: 0x10C8,
            throw_arithmetic: 0x1128,
            invoke_dispatch: 0x10D0,
            invoke_virtual_mic: 0x10D8,
            lambda_int_to_double: 0x10DC,
            write_barrier: 0x10E0,
            satb_pre_write_barrier: 0x1110,
            uncommon_trap: 0x10E8,
            math_fma_double: 0x10F0,
            math_fma_float: 0x10F8,
            tlab_cursor_offset_in_thread: 0,
            tlab_end_offset_in_thread: 8,
            class_id_offset_in_obj: 0,
            get_current_thread: 0x1100,
            tlab_post_init: 0x1108,
            frame_record: 0x1110,
            shadow_stack_offset_in_thread: 0,
            throw_exception: 0x1118,
            jit_npe_with_action: 0x1120,
            dispatch_threw: 0x1128,
            jit_frem: 0x1130,
            jit_drem: 0x1138,
            self_call_stack_guard: 0x1140,
            region_bounds_addr: 0x1148,
            native_stack_floor_fn: 0x1150,
            ldc_string: 0x1158,
            safepoint_flag_addr: 0x1160,
            safepoint_slow_path: 0x1168,
            jit_card_table_addr: 0x1170,
            jit_card_old_base: 0x1178,
            jit_card_old_end: 0x1180,
        }
    }

    // --- CachedBytecodeMethod ---

    #[test]
    fn test_cached_method_construction() {
        let m = make_cached_method();
        assert_eq!(m.declaring_class_id, ClassId::new(1));
        assert_eq!(&*m.class_name, "java/lang/Object");
        assert_eq!(&*m.method_name, "hashCode");
        assert_eq!(&*m.method_descriptor, "()I");
        assert_eq!(m.max_stack, 2);
        assert_eq!(m.max_locals, 1);
        assert_eq!(m.num_params, 0);
    }

    #[test]
    fn test_cached_method_source_file_some() {
        let m = make_cached_method();
        assert!(m.source_file.is_some());
        assert_eq!(&*m.source_file.unwrap(), "Object.java");
    }

    #[test]
    fn test_cached_method_source_file_none() {
        let m = CachedBytecodeMethod {
            source_file: None,
            ..make_cached_method()
        };
        assert!(m.source_file.is_none());
    }

    #[test]
    fn test_cached_method_code_content() {
        let m = make_cached_method();
        assert_eq!(m.code.len(), 1);
        assert_eq!(m.code[0], 0xB1); // return opcode
    }

    #[test]
    fn test_cached_method_empty_exception_table() {
        let m = make_cached_method();
        assert!(m.exception_table.is_empty());
    }

    #[test]
    fn test_cached_method_with_exception_table() {
        let entry = ExceptionTableEntry {
            start_pc: 0,
            end_pc: 10,
            handler_pc: 20,
            catch_type: 5,
        };
        let m = CachedBytecodeMethod {
            exception_table: Arc::from(vec![entry].as_slice()),
            ..make_cached_method()
        };
        assert_eq!(m.exception_table.len(), 1);
        assert_eq!(m.exception_table[0].start_pc, 0);
        assert_eq!(m.exception_table[0].end_pc, 10);
        assert_eq!(m.exception_table[0].handler_pc, 20);
        assert_eq!(m.exception_table[0].catch_type, 5);
    }

    #[test]
    fn test_cached_method_clone() {
        let m1 = make_cached_method();
        let m2 = m1.clone();
        assert_eq!(&*m1.class_name, &*m2.class_name);
        assert_eq!(&*m1.method_name, &*m2.method_name);
        assert_eq!(m1.max_stack, m2.max_stack);
        assert_eq!(m1.max_locals, m2.max_locals);
        assert_eq!(m1.code.len(), m2.code.len());
    }

    #[test]
    fn test_cached_method_arc_sharing() {
        let m1 = make_cached_method();
        let m2 = m1.clone();
        // Arc::clone shares the same allocation
        assert!(Arc::ptr_eq(&m1.class_name, &m2.class_name));
        assert!(Arc::ptr_eq(&m1.code, &m2.code));
    }

    #[test]
    fn test_cached_method_large_code() {
        let code: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
        let m = CachedBytecodeMethod {
            code: Arc::from(code.as_slice()),
            max_stack: 100,
            max_locals: 50,
            ..make_cached_method()
        };
        assert_eq!(m.code.len(), 1024);
        assert_eq!(m.max_stack, 100);
        assert_eq!(m.max_locals, 50);
    }

    // --- JitRuntimeHelpers ---

    #[test]
    fn test_helpers_construction() {
        let h = make_helpers();
        assert_eq!(h.newarray, 0x1000);
        assert_eq!(h.new_object, 0x1008);
        assert_eq!(h.write_barrier, 0x10E0);
    }

    #[test]
    fn test_helpers_copy() {
        let h1 = make_helpers();
        let h2 = h1; // Copy
        assert_eq!(h1.newarray, h2.newarray);
        assert_eq!(h1.invoke_dispatch, h2.invoke_dispatch);
    }

    #[test]
    fn test_helpers_clone() {
        let h1 = make_helpers();
        let h2 = h1.clone();
        assert_eq!(h1.checkcast, h2.checkcast);
        assert_eq!(h1.instanceof_check, h2.instanceof_check);
    }

    #[test]
    fn test_helpers_all_fields_distinct() {
        let h = make_helpers();
        let ptrs = [
            h.newarray,
            h.new_object,
            h.anewarray_object,
            h.baload,
            h.bastore,
            h.iaload,
            h.iastore,
            h.aaload,
            h.aastore,
            h.multianewarray_2d,
            h.arraylength,
            h.getfield,
            h.putfield_int,
            h.putfield_long,
            h.putfield_float,
            h.putfield_double,
            h.putfield_object,
            h.getstatic,
            h.putstatic_int,
            h.putstatic_long,
            h.putstatic_float,
            h.putstatic_double,
            h.putstatic_object,
            h.checkcast,
            h.instanceof_check,
            h.throw_aioobe,
            h.invoke_dispatch,
            h.invoke_virtual_mic,
            h.lambda_int_to_double,
            h.write_barrier,
            h.satb_pre_write_barrier,
        ];
        // All addresses should be unique
        let mut set = std::collections::HashSet::new();
        for p in &ptrs {
            assert!(set.insert(p), "Duplicate pointer value: {:#x}", p);
        }
        assert_eq!(set.len(), 31);
    }

    #[test]
    fn test_helpers_zero_values() {
        let h = JitRuntimeHelpers {
            newarray: 0,
            new_object: 0,
            anewarray_object: 0,
            baload: 0,
            bastore: 0,
            iaload: 0,
            iastore: 0,
            aaload: 0,
            aastore: 0,
            multianewarray_2d: 0,
            arraylength: 0,
            getfield: 0,
            putfield_int: 0,
            putfield_long: 0,
            putfield_float: 0,
            putfield_double: 0,
            putfield_object: 0,
            getstatic: 0,
            putstatic_int: 0,
            putstatic_long: 0,
            putstatic_float: 0,
            putstatic_double: 0,
            putstatic_object: 0,
            checkcast: 0,
            instanceof_check: 0,
            throw_aioobe: 0,
            throw_arithmetic: 0,
            invoke_dispatch: 0,
            invoke_virtual_mic: 0,
            lambda_int_to_double: 0,
            write_barrier: 0,
            satb_pre_write_barrier: 0,
            uncommon_trap: 0,
            math_fma_double: 0,
            math_fma_float: 0,
            tlab_cursor_offset_in_thread: 0,
            tlab_end_offset_in_thread: 0,
            class_id_offset_in_obj: 0,
            get_current_thread: 0,
            tlab_post_init: 0,
            frame_record: 0,
            shadow_stack_offset_in_thread: 0,
            throw_exception: 0,
            jit_npe_with_action: 0,
            dispatch_threw: 0,
            jit_frem: 0,
            jit_drem: 0,
            self_call_stack_guard: 0,
            region_bounds_addr: 0,
            native_stack_floor_fn: 0,
            ldc_string: 0,
            safepoint_flag_addr: 0,
            safepoint_slow_path: 0,
            jit_card_table_addr: 0,
            jit_card_old_base: 0,
            jit_card_old_end: 0,
        };
        assert_eq!(h.newarray, 0);
        assert_eq!(h.write_barrier, 0);
    }

    #[test]
    fn test_helpers_field_access_all() {
        let h = make_helpers();
        // Verify every field is accessible and has the expected value
        assert_eq!(h.anewarray_object, 0x1010);
        assert_eq!(h.baload, 0x1018);
        assert_eq!(h.bastore, 0x1020);
        assert_eq!(h.iaload, 0x1028);
        assert_eq!(h.iastore, 0x1030);
        assert_eq!(h.aaload, 0x1038);
        assert_eq!(h.aastore, 0x1040);
        assert_eq!(h.multianewarray_2d, 0x1048);
        assert_eq!(h.arraylength, 0x1050);
        assert_eq!(h.getfield, 0x1058);
        assert_eq!(h.putfield_int, 0x1060);
        assert_eq!(h.putfield_long, 0x1068);
        assert_eq!(h.putfield_float, 0x1070);
        assert_eq!(h.putfield_double, 0x1078);
        assert_eq!(h.putfield_object, 0x1080);
        assert_eq!(h.getstatic, 0x1088);
        assert_eq!(h.putstatic_int, 0x1090);
        assert_eq!(h.putstatic_long, 0x1098);
        assert_eq!(h.putstatic_float, 0x10A0);
        assert_eq!(h.putstatic_double, 0x10A8);
        assert_eq!(h.putstatic_object, 0x10B0);
        assert_eq!(h.checkcast, 0x10B8);
        assert_eq!(h.instanceof_check, 0x10C0);
        assert_eq!(h.throw_aioobe, 0x10C8);
        assert_eq!(h.invoke_dispatch, 0x10D0);
        assert_eq!(h.invoke_virtual_mic, 0x10D8);
    }

    #[test]
    fn test_cached_method_multiple_exception_entries() {
        let entries = vec![
            ExceptionTableEntry {
                start_pc: 0,
                end_pc: 5,
                handler_pc: 10,
                catch_type: 1,
            },
            ExceptionTableEntry {
                start_pc: 5,
                end_pc: 15,
                handler_pc: 20,
                catch_type: 2,
            },
            ExceptionTableEntry {
                start_pc: 0,
                end_pc: 15,
                handler_pc: 30,
                catch_type: 0,
            }, // finally
        ];
        let m = CachedBytecodeMethod {
            exception_table: Arc::from(entries.as_slice()),
            ..make_cached_method()
        };
        assert_eq!(m.exception_table.len(), 3);
        assert_eq!(m.exception_table[2].catch_type, 0); // finally handler
    }

    #[test]
    fn test_cached_method_declaring_class_id_equality() {
        let m1 = make_cached_method();
        let m2 = CachedBytecodeMethod {
            declaring_class_id: ClassId::new(2),
            ..make_cached_method()
        };
        assert_eq!(m1.declaring_class_id, ClassId::new(1));
        assert_eq!(m2.declaring_class_id, ClassId::new(2));
        assert_ne!(m1.declaring_class_id, m2.declaring_class_id);
    }

    // --- JitRuntimeHelpers validation ---

    #[test]
    fn test_helpers_validate_all_nonzero() {
        let h = make_helpers();
        assert!(h.validate().is_ok());
    }

    #[test]
    fn test_helpers_validate_fails_on_zero() {
        let mut h = make_helpers();
        h.newarray = 0;
        let err = h.validate().expect_err("zero newarray must fail");
        assert!(err.contains(&"newarray"));
    }

    #[test]
    fn test_helpers_null_pointers_empty_when_valid() {
        let h = make_helpers();
        assert!(h.null_pointers().is_empty());
    }

    #[test]
    fn test_helpers_null_pointers_reports_zeroed_fields() {
        let mut h = make_helpers();
        h.newarray = 0;
        h.write_barrier = 0;
        let nulls = h.null_pointers();
        assert!(nulls.contains(&"newarray"));
        assert!(nulls.contains(&"write_barrier"));
        assert_eq!(nulls.len(), 2);
    }

    // --- Builder tests deleted in 2026-05-16 audit: Builder itself
    //     was deleted (see comment above the deleted block in the
    //     module body). The remaining tests cover the runtime-helpers
    //     struct + validation directly.

    // --- JitRuntimeHelpers golden ABI offsets ---
    //
    // The JIT compiler bakes `JitRuntimeHelpers` field addresses into
    // generated RWX machine code as absolute CALL targets and as
    // `[helpers_ptr + disp32]` loads. A silent field reorder would
    // change the disp32 immediates while the JIT still emits the old
    // offsets — i.e. a CALL that used to dispatch `new_object` would
    // now dispatch `anewarray_object`, with no compile error. This
    // failure is undetectable at runtime until the wrong helper
    // corrupts the heap.
    //
    // The struct is `#[repr(C)]` and every field is `usize` (8 bytes
    // on x86-64, the only supported target). That gives a sequential
    // layout with no padding, so the documented offset of field N is
    // simply `N * 8`. The test below pins both:
    //
    //   1. The byte offset of every individual field (via
    //      `std::mem::offset_of!`, stable since 1.77), and
    //   2. The 0-based field index of every field (via a const N) so
    //      a swap of two fields with the same size — which preserves
    //      every offset individually — is also caught.
    //
    // If you add a new field to `JitRuntimeHelpers`, append a row
    // below and bump the `NUM_FIELDS` literal in the const-assert in
    // the module body. Do NOT renumber existing rows: the indices
    // here are part of the JIT ABI.

    /// Width of every field in `JitRuntimeHelpers` (every field is `usize`).
    /// On any 64-bit target Rust supports this is 8. We pin to 8 because the
    /// JIT only emits x86-64 code today; if/when 32-bit support lands, this
    /// test (and the JIT immediate-width assumptions) must be revisited.
    const FIELD_WIDTH: usize = 8;

    /// Documented JIT ABI offset of a field, given its 0-based index in
    /// the `#[repr(C)]` declaration order. Sequential because every
    /// field is `usize` (no inter-field padding).
    const fn expected_offset(index: usize) -> usize {
        index * FIELD_WIDTH
    }

    #[test]
    fn jit_runtime_helpers_field_size_is_eight() {
        // x86-64 only. If this fires, the per-field 8-byte stride
        // below is no longer the JIT ABI and the offset assertions
        // are stale.
        assert_eq!(std::mem::size_of::<usize>(), FIELD_WIDTH);
    }

    #[test]
    fn jit_runtime_helpers_struct_size_matches_field_count() {
        // `#[repr(C)]` + all `usize` fields ⇒ no padding ⇒ struct size
        // is exactly `NUM_FIELDS * 8`. Drift here means a field was
        // added/removed without updating `helper_fields!`, OR the
        // struct gained a non-`usize` field that violates the JIT ABI.
        assert_eq!(
            std::mem::size_of::<JitRuntimeHelpers>(),
            JitRuntimeHelpers::NUM_FIELDS * FIELD_WIDTH,
        );
        // And the macro-driven count is the canonical 56.
        assert_eq!(JitRuntimeHelpers::NUM_FIELDS, 56);
    }

    #[test]
    fn jit_runtime_helpers_repr_c_golden_offsets() {
        // Each row: (0-based ABI index, field name, offset_of! probe).
        // The expected byte offset is `index * 8`. The names are also
        // checked against `all_fields()` so a field rename (which the
        // offset_of! invocation would silently follow) is caught too.
        let probes: [(usize, &'static str, usize); JitRuntimeHelpers::NUM_FIELDS] = [
            (
                0,
                "newarray",
                std::mem::offset_of!(JitRuntimeHelpers, newarray),
            ),
            (
                1,
                "new_object",
                std::mem::offset_of!(JitRuntimeHelpers, new_object),
            ),
            (
                2,
                "anewarray_object",
                std::mem::offset_of!(JitRuntimeHelpers, anewarray_object),
            ),
            (3, "baload", std::mem::offset_of!(JitRuntimeHelpers, baload)),
            (
                4,
                "bastore",
                std::mem::offset_of!(JitRuntimeHelpers, bastore),
            ),
            (5, "iaload", std::mem::offset_of!(JitRuntimeHelpers, iaload)),
            (
                6,
                "iastore",
                std::mem::offset_of!(JitRuntimeHelpers, iastore),
            ),
            (7, "aaload", std::mem::offset_of!(JitRuntimeHelpers, aaload)),
            (
                8,
                "aastore",
                std::mem::offset_of!(JitRuntimeHelpers, aastore),
            ),
            (
                9,
                "multianewarray_2d",
                std::mem::offset_of!(JitRuntimeHelpers, multianewarray_2d),
            ),
            (
                10,
                "arraylength",
                std::mem::offset_of!(JitRuntimeHelpers, arraylength),
            ),
            (
                11,
                "getfield",
                std::mem::offset_of!(JitRuntimeHelpers, getfield),
            ),
            (
                12,
                "putfield_int",
                std::mem::offset_of!(JitRuntimeHelpers, putfield_int),
            ),
            (
                13,
                "putfield_long",
                std::mem::offset_of!(JitRuntimeHelpers, putfield_long),
            ),
            (
                14,
                "putfield_float",
                std::mem::offset_of!(JitRuntimeHelpers, putfield_float),
            ),
            (
                15,
                "putfield_double",
                std::mem::offset_of!(JitRuntimeHelpers, putfield_double),
            ),
            (
                16,
                "putfield_object",
                std::mem::offset_of!(JitRuntimeHelpers, putfield_object),
            ),
            (
                17,
                "getstatic",
                std::mem::offset_of!(JitRuntimeHelpers, getstatic),
            ),
            (
                18,
                "putstatic_int",
                std::mem::offset_of!(JitRuntimeHelpers, putstatic_int),
            ),
            (
                19,
                "putstatic_long",
                std::mem::offset_of!(JitRuntimeHelpers, putstatic_long),
            ),
            (
                20,
                "putstatic_float",
                std::mem::offset_of!(JitRuntimeHelpers, putstatic_float),
            ),
            (
                21,
                "putstatic_double",
                std::mem::offset_of!(JitRuntimeHelpers, putstatic_double),
            ),
            (
                22,
                "putstatic_object",
                std::mem::offset_of!(JitRuntimeHelpers, putstatic_object),
            ),
            (
                23,
                "checkcast",
                std::mem::offset_of!(JitRuntimeHelpers, checkcast),
            ),
            (
                24,
                "instanceof_check",
                std::mem::offset_of!(JitRuntimeHelpers, instanceof_check),
            ),
            (
                25,
                "throw_aioobe",
                std::mem::offset_of!(JitRuntimeHelpers, throw_aioobe),
            ),
            (
                26,
                "throw_arithmetic",
                std::mem::offset_of!(JitRuntimeHelpers, throw_arithmetic),
            ),
            (
                27,
                "invoke_dispatch",
                std::mem::offset_of!(JitRuntimeHelpers, invoke_dispatch),
            ),
            (
                28,
                "invoke_virtual_mic",
                std::mem::offset_of!(JitRuntimeHelpers, invoke_virtual_mic),
            ),
            (
                29,
                "lambda_int_to_double",
                std::mem::offset_of!(JitRuntimeHelpers, lambda_int_to_double),
            ),
            (
                30,
                "write_barrier",
                std::mem::offset_of!(JitRuntimeHelpers, write_barrier),
            ),
            (
                31,
                "satb_pre_write_barrier",
                std::mem::offset_of!(JitRuntimeHelpers, satb_pre_write_barrier),
            ),
            (
                32,
                "uncommon_trap",
                std::mem::offset_of!(JitRuntimeHelpers, uncommon_trap),
            ),
            (
                33,
                "math_fma_double",
                std::mem::offset_of!(JitRuntimeHelpers, math_fma_double),
            ),
            (
                34,
                "math_fma_float",
                std::mem::offset_of!(JitRuntimeHelpers, math_fma_float),
            ),
            (
                35,
                "tlab_cursor_offset_in_thread",
                std::mem::offset_of!(JitRuntimeHelpers, tlab_cursor_offset_in_thread),
            ),
            (
                36,
                "tlab_end_offset_in_thread",
                std::mem::offset_of!(JitRuntimeHelpers, tlab_end_offset_in_thread),
            ),
            (
                37,
                "class_id_offset_in_obj",
                std::mem::offset_of!(JitRuntimeHelpers, class_id_offset_in_obj),
            ),
            (
                38,
                "get_current_thread",
                std::mem::offset_of!(JitRuntimeHelpers, get_current_thread),
            ),
            (
                39,
                "tlab_post_init",
                std::mem::offset_of!(JitRuntimeHelpers, tlab_post_init),
            ),
            (
                40,
                "frame_record",
                std::mem::offset_of!(JitRuntimeHelpers, frame_record),
            ),
            (
                41,
                "shadow_stack_offset_in_thread",
                std::mem::offset_of!(JitRuntimeHelpers, shadow_stack_offset_in_thread),
            ),
            (
                42,
                "throw_exception",
                std::mem::offset_of!(JitRuntimeHelpers, throw_exception),
            ),
            (
                43,
                "jit_npe_with_action",
                std::mem::offset_of!(JitRuntimeHelpers, jit_npe_with_action),
            ),
            (
                44,
                "dispatch_threw",
                std::mem::offset_of!(JitRuntimeHelpers, dispatch_threw),
            ),
            (
                45,
                "jit_frem",
                std::mem::offset_of!(JitRuntimeHelpers, jit_frem),
            ),
            (
                46,
                "jit_drem",
                std::mem::offset_of!(JitRuntimeHelpers, jit_drem),
            ),
            (
                47,
                "self_call_stack_guard",
                std::mem::offset_of!(JitRuntimeHelpers, self_call_stack_guard),
            ),
            (
                48,
                "region_bounds_addr",
                std::mem::offset_of!(JitRuntimeHelpers, region_bounds_addr),
            ),
            (
                49,
                "native_stack_floor_fn",
                std::mem::offset_of!(JitRuntimeHelpers, native_stack_floor_fn),
            ),
            (
                50,
                "ldc_string",
                std::mem::offset_of!(JitRuntimeHelpers, ldc_string),
            ),
            (
                51,
                "safepoint_flag_addr",
                std::mem::offset_of!(JitRuntimeHelpers, safepoint_flag_addr),
            ),
            (
                52,
                "safepoint_slow_path",
                std::mem::offset_of!(JitRuntimeHelpers, safepoint_slow_path),
            ),
            (
                53,
                "jit_card_table_addr",
                std::mem::offset_of!(JitRuntimeHelpers, jit_card_table_addr),
            ),
            (
                54,
                "jit_card_old_base",
                std::mem::offset_of!(JitRuntimeHelpers, jit_card_old_base),
            ),
            (
                55,
                "jit_card_old_end",
                std::mem::offset_of!(JitRuntimeHelpers, jit_card_old_end),
            ),
        ];

        // (a) Each field is at its documented sequential byte offset.
        // A swap of two same-size fields would change the (index, name)
        // → offset_of! relationship and trip this.
        for (idx, name, actual) in probes.iter() {
            let expected = expected_offset(*idx);
            assert_eq!(
                *actual, expected,
                "ABI offset drift: field `{}` (index {}) is at byte offset {}, \
                 expected {}. The JIT bakes this offset into RWX code — fix the \
                 reorder, or if intentional, update both this table and \
                 `vm/src/jit/helpers.rs::build_helpers`.",
                name, idx, actual, expected,
            );
        }

        // (b) The macro-generated name list agrees with the golden
        // table, in the same order. This catches a field rename in
        // the struct that the contributor forgot to mirror into the
        // `helper_fields!` macro invocation (or vice versa).
        let h = make_helpers();
        let fields = h.all_fields();
        assert_eq!(fields.len(), probes.len());
        for (i, (_, expected_name, _)) in probes.iter().enumerate() {
            assert_eq!(
                fields[i].name, *expected_name,
                "macro-driven field order disagrees with golden ABI table at \
                 index {}: macro has `{}`, golden has `{}`",
                i, fields[i].name, expected_name,
            );
        }
    }

    #[test]
    fn jit_runtime_helpers_all_fields_classified() {
        // The macro must classify every field. 41 RequiredPtr + 6
        // 41 RequiredPtr + 6 OptionalPtr + 9 Offset = 56. A new
        // field whose classification is omitted will fail to compile (the
        // macro requires both arms); this test pins the *counts* so a
        // reclassification (e.g. demoting a RequiredPtr to OptionalPtr) is
        // also a deliberate, reviewed change.
        let h = make_helpers();
        let f = h.all_fields();
        let req = f
            .iter()
            .filter(|e| e.kind == FieldKind::RequiredPtr)
            .count();
        let opt = f
            .iter()
            .filter(|e| e.kind == FieldKind::OptionalPtr)
            .count();
        let off = f.iter().filter(|e| e.kind == FieldKind::Offset).count();
        assert_eq!(req, 41, "required-pointer count drifted");
        assert_eq!(opt, 6, "optional-pointer count drifted");
        assert_eq!(off, 9, "offset-field count drifted");
        assert_eq!(req + opt + off, JitRuntimeHelpers::NUM_FIELDS);
    }

    #[test]
    fn jit_runtime_helpers_validate_ignores_offsets() {
        // `class_id_offset_in_obj` is 0 by contract; the validator
        // must NOT reject on that. (Regression for the round-9 fix —
        // the previous `validate()` looped over a hand-maintained array
        // that omitted all offset/optional fields, so this was true
        // by accident. The new validator iterates ALL 46 fields and
        // must still pass when offsets are zero.)
        let mut h = make_helpers();
        h.tlab_cursor_offset_in_thread = 0;
        h.tlab_end_offset_in_thread = 0;
        h.class_id_offset_in_obj = 0;
        assert!(h.validate().is_ok());
        assert!(h.null_pointers().is_empty());
    }

    #[test]
    fn jit_runtime_helpers_validate_accepts_unwired_optionals() {
        // Optional helpers default to zero ("not wired") — must pass.
        let mut h = make_helpers();
        h.get_current_thread = 0;
        h.tlab_post_init = 0;
        assert!(h.validate().is_ok());
        // And they are NOT reported by null_pointers (they are not
        // mandatory-and-null, just optional-and-unset).
        assert!(h.null_pointers().is_empty());
    }

    #[test]
    fn jit_runtime_helpers_validate_rejects_each_required_null() {
        // Sweep every required-pointer field: zeroing it must trip
        // the validator AND report the field by name.
        let names: Vec<&'static str> = make_helpers()
            .all_fields()
            .iter()
            .filter(|e| e.kind == FieldKind::RequiredPtr)
            .map(|e| e.name)
            .collect();
        assert_eq!(names.len(), 41);
        for name in names {
            let mut h = make_helpers();
            // Zero the field by name via a match — the macro doesn't
            // give us per-field mutable accessors, so this is exhaustive
            // but verbose. The test exists precisely to ensure no
            // required field is silently uncovered.
            zero_field_by_name(&mut h, name);
            match h.validate() {
                Ok(()) => panic!(
                    "validator failed to reject null required field `{}` (returned Ok)",
                    name
                ),
                Err(err) => assert!(
                    err.contains(&name),
                    "validator's Err list did not include `{}`; got {:?}",
                    name,
                    err,
                ),
            }
            let nulls = h.null_pointers();
            assert!(
                nulls.contains(&name),
                "null_pointers() did not report `{}`; got {:?}",
                name,
                nulls,
            );
        }
    }

    #[test]
    fn jit_runtime_helpers_all_required_null_reports_every_name() {
        // Zero EVERY required pointer at once: `null_pointers()` must
        // return the complete set of 41 required-field names (and
        // `validate()` must reject). This complements the per-field
        // sweep above — it proves the validator does not stop at the
        // first miss and that the offset/optional fields (left non-zero
        // for offsets, zero for unwired optionals) never leak into the
        // report.
        let mut h = make_helpers();
        let required: Vec<&'static str> = h
            .all_fields()
            .iter()
            .filter(|e| e.kind == FieldKind::RequiredPtr)
            .map(|e| e.name)
            .collect();
        assert_eq!(required.len(), 41, "expected 41 required pointers");
        // throw_exception is the round-10 addition — pin it explicitly so
        // a regression that drops it from the required set is caught here
        // and not just by the count.
        assert!(
            required.contains(&"throw_exception"),
            "throw_exception must be a required (null-rejected) pointer",
        );
        // jit_npe_with_action is the JEP-358 inline-NPE-path addition — pin it
        // explicitly for the same reason.
        assert!(
            required.contains(&"jit_npe_with_action"),
            "jit_npe_with_action must be a required (null-rejected) pointer",
        );
        // dispatch_threw is the i64::MIN-sentinel J/D-return disambiguation
        // helper — pin it explicitly for the same reason.
        assert!(
            required.contains(&"dispatch_threw"),
            "dispatch_threw must be a required (null-rejected) pointer",
        );
        // jit_frem / jit_drem are the IR FP-tier (Slice A) fmod helpers — pin
        // them explicitly for the same reason.
        assert!(
            required.contains(&"jit_frem"),
            "jit_frem must be a required (null-rejected) pointer",
        );
        assert!(
            required.contains(&"jit_drem"),
            "jit_drem must be a required (null-rejected) pointer",
        );
        // ldc_string is the interned-String-materialization helper for
        // compiled `ldc` sites — pin it explicitly for the same reason.
        assert!(
            required.contains(&"ldc_string"),
            "ldc_string must be a required (null-rejected) pointer",
        );

        for name in &required {
            zero_field_by_name(&mut h, name);
        }

        let nulls = h.null_pointers();
        assert_eq!(
            nulls.len(),
            required.len(),
            "null_pointers() should report all {} required fields; got {:?}",
            required.len(),
            nulls,
        );
        for name in &required {
            assert!(
                nulls.contains(name),
                "null_pointers() omitted required field `{}`; got {:?}",
                name,
                nulls,
            );
        }
        // And validate() must fail with the same complete list.
        match h.validate() {
            Ok(()) => panic!("validate() returned Ok with all required pointers null"),
            Err(err) => assert_eq!(err.len(), required.len()),
        }
    }

    // Helper for `validate_rejects_each_required_null`: zero a single
    // field by string name. Kept in the test module so production
    // code doesn't grow a stringly-typed mutator.
    fn zero_field_by_name(h: &mut JitRuntimeHelpers, name: &str) {
        match name {
            "newarray" => h.newarray = 0,
            "new_object" => h.new_object = 0,
            "anewarray_object" => h.anewarray_object = 0,
            "baload" => h.baload = 0,
            "bastore" => h.bastore = 0,
            "iaload" => h.iaload = 0,
            "iastore" => h.iastore = 0,
            "aaload" => h.aaload = 0,
            "aastore" => h.aastore = 0,
            "multianewarray_2d" => h.multianewarray_2d = 0,
            "arraylength" => h.arraylength = 0,
            "getfield" => h.getfield = 0,
            "putfield_int" => h.putfield_int = 0,
            "putfield_long" => h.putfield_long = 0,
            "putfield_float" => h.putfield_float = 0,
            "putfield_double" => h.putfield_double = 0,
            "putfield_object" => h.putfield_object = 0,
            "getstatic" => h.getstatic = 0,
            "putstatic_int" => h.putstatic_int = 0,
            "putstatic_long" => h.putstatic_long = 0,
            "putstatic_float" => h.putstatic_float = 0,
            "putstatic_double" => h.putstatic_double = 0,
            "putstatic_object" => h.putstatic_object = 0,
            "checkcast" => h.checkcast = 0,
            "instanceof_check" => h.instanceof_check = 0,
            "throw_aioobe" => h.throw_aioobe = 0,
            "throw_arithmetic" => h.throw_arithmetic = 0,
            "invoke_dispatch" => h.invoke_dispatch = 0,
            "invoke_virtual_mic" => h.invoke_virtual_mic = 0,
            "lambda_int_to_double" => h.lambda_int_to_double = 0,
            "write_barrier" => h.write_barrier = 0,
            "satb_pre_write_barrier" => h.satb_pre_write_barrier = 0,
            "uncommon_trap" => h.uncommon_trap = 0,
            "math_fma_double" => h.math_fma_double = 0,
            "math_fma_float" => h.math_fma_float = 0,
            // round-10: throw_exception was added as a RequiredPtr after
            // the original 33-field sweep was written. Without this arm the
            // sweep panics on it and the required-null coverage is silently
            // incomplete (the exact failure mode this test guards against).
            "throw_exception" => h.throw_exception = 0,
            "jit_npe_with_action" => h.jit_npe_with_action = 0,
            "dispatch_threw" => h.dispatch_threw = 0,
            "jit_frem" => h.jit_frem = 0,
            "jit_drem" => h.jit_drem = 0,
            "ldc_string" => h.ldc_string = 0,
            other => panic!("unknown required-pointer field name in test: {}", other),
        }
    }
}
