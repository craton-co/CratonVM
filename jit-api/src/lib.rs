// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT compiler API types for CratonVM.
//!
//! Shared types used by the JIT compiler crate and the VM crate:
//! - [`CachedBytecodeMethod`] — method data needed for JIT compilation
//! - [`JitRuntimeHelpers`] — function pointer table for JIT runtime callbacks
//! - [`helpers_abi`] — the typed ABI description of that table: one
//!   `HelperFn*` signature alias per callable slot, the machine-readable
//!   [`HELPER_FIELDS`] descriptor list, null-checked accessors, and
//!   [`JitRuntimeHelpers::validate_with`]
//! - [`gpu_lowering::GpuLowering`] (under the `gpu-lowering` feature) —
//!   trait seam for emitting PTX from a resolved Java method.
//!
//! # SAFETY: the `JitRuntimeHelpers` ABI is frozen
//!
//! [`JitRuntimeHelpers`] is a `#[repr(C)]` table of bare `usize` words. The
//! backend bakes each slot's **absolute address** into RWX machine code and
//! hand-writes the argument-register setup at every call site; the byte offsets
//! are what `as_words`/`word_at` and any non-Rust producer depend on. Never
//! reorder, remove, retype, or insert a field; append only, and bump
//! [`JIT_HELPERS_ABI_VERSION`] when you do — [`helpers_abi::ABI_REVISIONS`]
//! makes that a compile error to forget. The full contract, the per-field
//! signatures, and the per-field nullability rules live in [`helpers_abi`];
//! `docs/jit/helper-abi.md` lists which invariants have tripwires, which
//! do not, and the procedure for adding a slot.
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

pub mod helpers_abi;

pub use helpers_abi::{
    accessor_name_matches_field, helper_field, helper_field_offset, helper_sig, helper_sig_of,
    str_eq, HelperAbiError, HelperAbiRevision, HelperArgAbi, HelperFieldDesc, HelperFnSig,
    HelperKind, HelperRetAbi, ABI_REVISIONS, GOLDEN_HELPER_OFFSETS,
    HELPERS_NEEDING_WIN64_STACK_ARGS, HELPER_FIELDS, HELPER_FIELD_STRIDE, HELPER_FN_SIGS,
    JIT_HELPERS_ABI_ALIGN, JIT_HELPERS_ABI_SIZE, JIT_HELPERS_ABI_VERSION, MAX_PLAUSIBLE_OFFSET,
    NUM_HELPER_FIELDS, SYSV_INT_ARG_REGS, WIN64_INT_ARG_REGS,
};

use std::sync::Arc;

use cratonvm_types::ClassId;

pub use cratonvm_reader::cached_method::*;

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
    /// `invokevirtual` / `invokeinterface` / `invokespecial` on a null
    /// RECEIVER, recorded by every helper that detects one: the generic
    /// `jit_invoke_dispatch`, the monomorphic `jit_invoke_virtual_mic`, and the
    /// seven direct-bound intrinsics that stand in for one of these calls.
    ///
    /// Unlike every code above it this one names no fixed message — HotSpot's
    /// action half is `Cannot invoke "Owner.name(sig)"`, which depends on the
    /// call site — so `jit_action_message` maps it to `None` and it changes no
    /// fallback. What it carries is the OPCODE FAMILY that trapped, which is
    /// what lets `runtime::interpreter::jit_npe_message` corroborate the bci it
    /// recovered from the frame's safepoint-id slot before reading an opcode
    /// there. The dispatch call is a published safepoint (the emitter stores
    /// `cur_bc_pc` immediately before it and files an oop map), so that slot
    /// holds THIS invoke's bci while the helper runs.
    pub const INVOKE_RECEIVER: u8 = 18;
    /// `monitorenter` on a null reference, recorded by `jit_monitor_enter`
    /// (JIT round 20, lane throw). Like [`INVOKE_RECEIVER`] it maps to no
    /// fixed fallback message: it corroborates the bci the monitor call's
    /// published safepoint left in the frame (both tiers publish the map
    /// before the helper call), so the drain rebuilds HotSpot's
    /// `Cannot enter synchronized block because "..." is null` from the
    /// bytecode there.
    pub const MONITOR_ENTER: u8 = 19;
    /// `monitorexit` on a null reference (`jit_monitor_exit`), as
    /// [`MONITOR_ENTER`] (`Cannot exit synchronized block ...`).
    pub const MONITOR_EXIT: u8 = 20;
    /// `athrow` on a null reference, recorded by `jit_throw_exception`'s null
    /// arm (JIT round 20 wave 7, lane fuzzC; probe `R20FuzzCAthrowNull1`). As
    /// [`MONITOR_ENTER`] it maps to no fixed fallback message: it corroborates
    /// the bci the snapshot's innermost frame carries (both x64 tiers pass the
    /// `athrow`'s own bci, which `apply_athrow_trap_bci` puts on that frame),
    /// so the drain rebuilds HotSpot's
    /// `Cannot throw exception because "..." is null` from the bytecode there.
    pub const ATHROW: u8 = 21;
}
/// Bit the JIT may set in `jit_getfield`'s `field_index` argument to say
/// **"this receiver is already proven to be an oop"**.
///
/// When set, the helper skips its `is_object_address` heap-membership walk.
/// Everything else — the pending-NPE contract, the slot bounds check, the
/// compact/legacy layout split, the read and the reference decode — is
/// unchanged, so this carries no new colouring or layout exposure. It is
/// purely "skip one validation".
///
/// # Why a flag bit and not a second helper slot
///
/// `helpers_abi.rs` pins [`JitRuntimeHelpers`]'s field count, byte size and
/// golden offsets with const assertions plus an ABI version, precisely so the
/// offsets the JIT bakes cannot move. A one-bit argument flag needs none of
/// that. `field_index` is a small non-negative slot index — a class-file field
/// table is `u16`-sized — so bit 62 cannot collide with a real index.
///
/// # Why skipping the walk is sound
///
/// The walk is validation against a stale/garbage receiver from a miscompiled
/// frame. The emitter sets this only where the IR's type lattice types the base
/// node `IrType::Ref` — the same proof the PRIMITIVE trusted-oop arm already
/// relies on, and that arm goes further and performs a raw inline load off this
/// very receiver. `plausible_heap_pointer` still runs either way, so null and
/// unaligned/out-of-range bits are still refused.
pub const GETFIELD_RECEIVER_PROVEN_OOP: u64 = 1 << 62;

/// Bit the JIT sets in `jit_getfield`'s `field_index` argument to say **"the
/// value I am about to receive is a REFERENCE, and I will dereference it"**.
///
/// Without it the helper cannot know what the caller asked for. It reads a
/// `Value` out of the slot and returns the payload of whichever variant it
/// finds, so a reference field whose slot has been type-punned to a primitive
/// comes back as that primitive's bits — and compiled code, which emitted a
/// plain `MOV r32,[reg+4]` for the `arraylength` that follows, dereferences an
/// integer.
///
/// That is not hypothetical. `org.apache.derby.iapi.types.SQLChar.rawData` is
/// declared `[C` and was read back as `Int(1)`; the emitted code went
///
/// ```text
///   call  jit_getfield          ; -> rax = 1
///   mov   r10, 8000000000000000h
///   cmp   rax, r10              ; the ONLY value it checks for
///   je    <deopt>
///   mov   eax,[rax+4]           ; arraylength  ->  SIGSEGV at addr 0x5
/// ```
///
/// i64::MIN was the only rejected value, so every other primitive payload
/// became a wild pointer.
///
/// With this bit set, a primitive found in a slot the caller will dereference
/// is degraded to null rather than returned — the same "degrade rather than
/// hand the JIT bits it will later deref → SIGSEGV" policy
/// `jit_decode_ref_word` already applies to an implausible POINTER, extended
/// to the case where the slot does not hold a pointer at all.
///
/// Bit 61 for the same reason bit 62 was chosen: a class-file field table is
/// `u16`-sized, so a real slot index cannot reach either.
pub const GETFIELD_EXPECT_REFERENCE: u64 = 1 << 61;

/// Where the NPE trap-SITE key sits in `jit_getfield`'s `field_index`
/// argument, and how wide it is.
///
/// The key is what lets a null receiver at a `getfield` carry HotSpot's JEP 358
/// message out of compiled code. The helper's null arm has the receiver (null)
/// and the slot index, and neither names the FIELD or the bci — so on its own
/// it can only raise an unmessaged `NullPointerException`, which is exactly
/// what it did until 2026-09-11. The emitter knows both; it records them as a
/// trap site (`x64::inlining::record_npe_trap_site`) and passes the key here,
/// and `runtime::interpreter::jit_npe_message` reads the opcode back out of the
/// trapping method's own bytecode.
///
/// Bits 32..56, i.e. above any real slot index (a class-file field table is
/// `u16`-sized) and below the two flags at 61/62. `0` means "no site
/// described", which is what every emitter arm without a bci passes and what
/// the feature's kill switch produces.
pub const GETFIELD_NPE_SITE_SHIFT: u32 = 32;

/// The 24 bits [`GETFIELD_NPE_SITE_SHIFT`] names. 24 because
/// `record_npe_trap_site` caps its ids at 24 bits for the same reason: the
/// inline-null-check trampolines carry a key in one imm32 beside an action
/// byte.
pub const GETFIELD_NPE_SITE_MASK: u64 = 0x00ff_ffff << GETFIELD_NPE_SITE_SHIFT;

/// Every bit in `jit_getfield`'s `field_index` argument that is a flag rather
/// than part of the index. Masked off in one place so a third flag cannot be
/// added without the strip site seeing it.
pub const GETFIELD_FLAG_BITS: u64 = GETFIELD_RECEIVER_PROVEN_OOP
    | GETFIELD_EXPECT_REFERENCE
    | GETFIELD_NPE_SITE_MASK
    | GETFIELD_IMPLICIT_NULL_SITE
    | GETFIELD_NO_KEEPALIVE;

/// Set by the IR lowering on the `jit_getfield` call of a COMPARE-ONLY
/// `Reference.referent` read -- the `refersTo` expansion's load and the
/// `ThreadLocal` expansion's `refersTo(this)` step (`ir::Graph::
/// no_keepalive_referent_loads`): the helper skips its slot-0 SATB keep-alive
/// (`ref_discovery::keep_alive_referent_read_raw`), as HotSpot loads the
/// referent of `Reference.refersTo0` / `PhantomReference.refersTo0`
/// `AS_NO_KEEPALIVE`. The ZGC resurrection gate still runs: a `refersTo` must
/// compare against the blocked answer, as the natives'
/// `get_field_no_keepalive` does. t1 wave 10, lane REFS3
/// (`CRATONVM_JIT_IR_REFERS_TO_NO_KEEPALIVE`, default ON; never set with the
/// switch at `0`).
///
/// Bit 59: free between the trap-site key at 32..56 and the flag at 60.
pub const GETFIELD_NO_KEEPALIVE: u64 = 1 << 59;

/// Did the emitter mark this `jit_getfield` argument as a compare-only
/// referent read ([`GETFIELD_NO_KEEPALIVE`])?
#[must_use]
pub const fn getfield_is_no_keepalive(arg: i64) -> bool {
    (arg as u64) & GETFIELD_NO_KEEPALIVE != 0
}

/// Set by the single-pass emitter on the `jit_getfield` call of a slow path
/// that is ALSO an implicit null check's recovery address
/// (`x64::Compiler::bind_implicit_null_recovery`): a null receiver reaching the
/// helper with this bit arrived through a translated hardware fault, not an
/// explicit `TEST`/`JZ`. The helper counts such arrivals per `(method, bci)` in
/// the VM's de-spec registry and, past the per-bci trap limit, has the method
/// recompiled with an explicit check there (HotSpot's
/// `too_many_traps(Reason_null_check)`). JIT round 20 wave 12, lane qNull
/// (proposal L1W19-2, `CRATONVM_JIT_NULL_TRAP_RECOMPILE`; never set with the
/// switch at `0`).
///
/// Bit 60: free beside the two flags at 61/62 and above the 24-bit trap-site
/// key at 32..56.
pub const GETFIELD_IMPLICIT_NULL_SITE: u64 = 1 << 60;

/// Did the emitter mark this `jit_getfield` argument as an implicit null
/// check's slow path ([`GETFIELD_IMPLICIT_NULL_SITE`])?
#[must_use]
pub const fn getfield_is_implicit_null_site(arg: i64) -> bool {
    (arg as u64) & GETFIELD_IMPLICIT_NULL_SITE != 0
}

/// Build `jit_getfield`'s third argument.
///
/// Seven call sites across the two backends emit this argument, and before
/// this existed each of them wrote `field_index as u64` by hand. That is how
/// [`GETFIELD_EXPECT_REFERENCE`] would rot: a site added later, or one whose
/// author did not know a safety bit had appeared, silently opts out of it and
/// the hole reopens at exactly one `getfield` arm. One encoder means a new flag
/// reaches every site by construction.
///
/// `npe_trap_key` is the id `x64::inlining::record_npe_trap_site` issued for
/// this site, or `0` for "not described" — see [`GETFIELD_NPE_SITE_SHIFT`]. It
/// rides in the same argument for the same reason the two flags do: the helper
/// table's field count, byte size and offsets are pinned by const assertions in
/// `helpers_abi.rs`, and spare bits in an argument need none of that.
pub const fn getfield_index_arg(
    field_index: u32,
    is_reference: bool,
    receiver_proven_oop: bool,
    npe_trap_key: u32,
) -> u64 {
    let mut arg = field_index as u64;
    if is_reference {
        arg |= GETFIELD_EXPECT_REFERENCE;
        // The receiver proof is only consulted on the reference path, and only
        // ever as an optimisation — see `GETFIELD_RECEIVER_PROVEN_OOP`.
        if receiver_proven_oop {
            arg |= GETFIELD_RECEIVER_PROVEN_OOP;
        }
    }
    // Masked, not asserted: `record_npe_trap_site` already refuses to issue an
    // id at or above 2^24, and a `const fn` cannot panic usefully anyway. A key
    // that somehow exceeded the field would otherwise spill into the slot index
    // and index the object out of bounds.
    arg |= ((npe_trap_key as u64) << GETFIELD_NPE_SITE_SHIFT) & GETFIELD_NPE_SITE_MASK;
    arg
}

/// Recover the NPE trap-site key from [`getfield_index_arg`]'s result. `0` when
/// the site was not described.
#[must_use]
pub const fn getfield_npe_site_of(arg: i64) -> u32 {
    // Truncation: the field is 24 bits wide by construction.
    (((arg as u64) & GETFIELD_NPE_SITE_MASK) >> GETFIELD_NPE_SITE_SHIFT) as u32
}

/// Recover the slot index from [`getfield_index_arg`]'s result — the DECODER
/// twin of that encoder.
///
/// One encoder means a new flag reaches every emitter by construction. It does
/// nothing at all for the other side, and the other side is where the flags are
/// dangerous: a decoder that misses one indexes an object with a bit set at 61
/// or 62, which is not a bounds-check failure, it is `idx * SLOT_SIZE`
/// overflowing.
///
/// That is not hypothetical twice over. `jit/src/x64/tests.rs`'s `stub_getfield`
/// checked `field_index as u32` against the slot count and then indexed with
/// `field_index as usize`: the u32 truncation dropped the flag, the bounds
/// check passed, and the index walked off the object — "which aborted the whole
/// test binary the first time a reference load carried a flag" (2026-08-23).
/// `jit/tests/ir_vs_singlepass.rs`'s twin stub was not updated with it and did
/// the same thing four days later, aborting the process inside an
/// `extern "C"` fn — where a panic cannot unwind, so it took every test after
/// it in the file with it.
///
/// Both of those are decode sites that had to *know* to strip. This is the one
/// place that knows, so a third flag added to [`GETFIELD_FLAG_BITS`] reaches
/// them without anyone remembering.
#[must_use]
pub const fn getfield_index_of(arg: i64) -> i64 {
    (arg as u64 & !GETFIELD_FLAG_BITS) as i64
}

#[cfg(test)]
mod getfield_arg_tests;

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
///
/// `Default` (every field zero) exists for the integration tests in
/// `jit/tests`, which build this table literally and wire up only the helpers
/// they exercise. They spread `..Default::default()` so that adding a field
/// here cannot break all of them at once — it did, for `service_callee_deopt`
/// and `set_throw_bci`.
///
/// Zero is the right default only for the fields whose contract already reads
/// 0 as unwired (`get_current_thread`, `safepoint_flag_addr`, the TLAB and
/// card-table offsets, …), where the backend checks for it and falls back.
/// It is NOT inert for a field the backend reaches via `emit_call_absolute`:
/// there a 0 is a CALL to a null pointer. If you add a call-target helper,
/// expect the `jit/tests` tables to need it wired explicitly to their local
/// trap stub, exactly as `set_throw_bci` / `service_callee_deopt` are.
/// Production builds always go through `build_helpers`, which populates every
/// field explicitly.
///
/// # SAFETY: frozen binary layout
///
/// This struct is an ABI, not a data structure. The rules are absolute:
///
/// * **`#[repr(C)]` is mandatory.** Backends read a slot by field name at
///   compile time and bake its value into a `CALL`, but `HELPER_FIELDS`, the
///   `Offset` slots and every golden-offset check name slots by byte offset.
///   `repr(Rust)` may reorder fields, which would silently re-point each of
///   those at a different helper.
/// * **Every field is `usize`** (8 bytes; x86-64 only), so the byte offset of
///   field *N* is exactly `N * 8`. A non-`usize` field would introduce padding
///   and break that identity — see the `const _` assertions below.
/// * **Append only.** Never reorder, never remove, never retype, never insert
///   in the middle. New helpers go at the end, exactly as `set_throw_bci` and
///   `service_callee_deopt` did.
/// * **Bump [`JIT_HELPERS_ABI_VERSION`] on any shape change**, including a
///   pure append: once a field exists, a producer and a consumer built against
///   different revisions cannot be distinguished by size alone.
/// * **Nullability is per field**, recorded in [`HELPER_FIELDS`]. A `required`
///   slot is `CALL`ed unconditionally and must be non-zero; a non-required
///   slot reads `0` as the documented "not wired" sentinel and the backend
///   falls back (or emits nothing).
///
/// The raw `usize` type conveys none of that. Use the typed view in
/// [`helpers_abi`] — [`Self::validate_with`] to check a table, the
/// `<field>_fn` accessors to obtain a null-checked, correctly-typed function
/// pointer, and [`HELPER_FIELDS`] to describe the layout to tooling.
#[derive(Clone, Copy, Debug, Default)]
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
    /// `extern "C" fn(vm_ptr: i64, ctor_receiver_or_zero: i64) -> i64`.
    /// A constructor site passes argument zero so a failed entry can withdraw
    /// its uninitialized finalizer registration; all other sites pass zero.
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
    /// `extern "C" fn(flag_addr: i64, body_id: i64) -> i64` — the `stw_requested` byte the poll tested
    /// (`vm/src/jit/helpers.rs::jit_safepoint_slow_path`), and the polling
    /// body's compile id (0 = unnamed; since interpreter round i1 wave 17,
    /// checked by the VM against the thread's compile-id mirror, so a backend
    /// that leaves the register undefined is answered as unnamed). The result is a
    /// verdict, non-zero when the calling frame must run interpreted now; only
    /// back-edge and self-tail polls read it (they then leave through an
    /// OSR-exit map). Called only on a
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
    /// Address of the generational card table's READ-ONLY JIT view
    /// (`cratonvm_gc::card_table::JitCardView`: magic, old_base, old_end,
    /// negated card-map address, flags).
    ///
    /// gen r4w4/cards4 (2026-09-24): this used to be the raw card-byte array,
    /// which compiled code stored into directly; handing that out disarms the
    /// card table's scan bound and summary map. The view is read, never
    /// stored through: the x64 inline barrier (`CRATONVM_JIT_INLINE_CARD_MARK`)
    /// tests the card byte and calls `write_barrier` for every clean→dirty
    /// transition, and the `aastore` arms read its precise-array flag. Same
    /// word, same position, new meaning; `0` (G1, ZGC, hand-built test tables)
    /// still means "no generational card metadata".
    pub jit_card_table_addr: usize,
    /// Inclusive old-generation base covered by the card view (informational:
    /// the emitted barrier loads the live value from the view).
    pub jit_card_old_base: usize,
    /// Exclusive old-generation end covered by the card view (informational,
    /// as above).
    pub jit_card_old_end: usize,
    /// `extern "C" fn(bci: i64)` — stamp the *currently executing compiled
    /// method's own* throw-site bci onto the pending-exception signal.
    ///
    /// Emitted on the cold side of every post-invoke exception check
    /// (`jit/src/x64.rs::emit_exception_check_stubs`). Without it the signal
    /// still carries the bci that the *callee's* compiled `athrow` lowering
    /// stashed, and the interpreter range-checks that foreign pc against THIS
    /// method's exception table — silently dropping a `finally` (a catch-all
    /// entry cannot be rescued by the type check the way a typed handler can).
    /// Appended at the END of the struct so all prior golden offsets stay
    /// stable.
    pub set_throw_bci: usize,
    /// Service a compiled callee's `i64::MIN` deopt/exception sentinel at an
    /// INLINE (generated) call site.
    ///
    /// The MIC/PIC cascade calls a cached compiled entry directly, so no
    /// dispatch helper is on the stack to notice that the callee trapped. The
    /// callee's reconstructed frame would then sit in the thread's single stash
    /// slot until some unrelated sink consumed it, de-speculating the wrong
    /// method and failing to resume. Called ONLY on the rare
    /// `RAX == i64::MIN` branch after an inline call.
    ///
    /// Appended at the END of the struct so all prior golden offsets stay
    /// stable.
    ///
    /// Signature: `extern "C" fn(vm_ptr: i64, info_ptr: i64, args_ptr: i64,
    /// num_args: i64) -> i64` — returns the resumed call result, or `i64::MIN`
    /// unchanged when the sentinel must keep propagating.
    pub service_callee_deopt: usize,
    /// Constant-pool-indexed `new` (0xbb) slow path — `extern "C"
    /// fn(vm_ptr: i64, holder_class_id: i64, cp_idx: i64) -> i64`.
    ///
    /// The ordinary [`Self::new_object`] path takes an already-resolved
    /// `(class_id, num_fields)` pair, which the compiler can only supply when
    /// the `new`'s target class is ALREADY LOADED. A hot method whose only
    /// un-taken branch does `throw new SomeException(...)` therefore failed to
    /// compile at all — `resolve_jit_new_site` returned `None` and the whole
    /// compile bailed, permanently after `MAX_TIER_FAIL_RETRIES`
    /// (jit-compile-bail-unresolved-new-cold-class.md).
    ///
    /// This helper moves resolution to run time: the compiler bakes the
    /// *referencing* class id and the CP index, and the helper resolves +
    /// initialises the target exactly like the interpreter's 0xbb handler
    /// (same thread, same program point) before falling into
    /// `jit_new_object`'s body. Sound because it is what the interpreter
    /// already does; free on the hot path because a `new` whose class IS
    /// loaded at compile time still takes the inline-TLAB/`new_object` path.
    ///
    /// `0` = not wired (hand-built test tables) → the backend refuses the
    /// deferred site and bails the compile, i.e. the pre-fix behaviour.
    /// Appended at the END of the struct so all prior golden offsets stay
    /// stable.
    pub new_object_cp: usize,
    /// Constant-pool-indexed `anewarray` (0xbd) slow path — `extern "C"
    /// fn(vm_ptr: i64, holder_class_id: i64, cp_idx: i64, length: i64) -> i64`.
    ///
    /// The `anewarray` sibling of [`Self::new_object_cp`]: same root cause
    /// (the component class is not loaded yet, so the compile-time resolver
    /// cannot name a class id), same runtime-resolution answer. `0` = not
    /// wired → the deferred site bails the compile. Appended at the END of
    /// the struct so all prior golden offsets stay stable.
    pub anewarray_object_cp: usize,
    /// `monitorenter` for compiled code. Returns the possibly-REMAPPED
    /// object, which the caller must store back: a contended acquire parks
    /// the thread and the object can move while it is parked.
    ///
    /// Optional - a table leaving this 0 makes `ir_lower` refuse a graph
    /// containing monitor ops rather than emit nothing and drop the lock.
    pub monitor_enter: usize,
    /// `monitorexit`. See `monitor_enter`.
    pub monitor_exit: usize,
    /// Constant-pool-indexed `ldc <Class>` (0x12/0x13 whose CP entry is a
    /// `CONSTANT_Class`) — `extern "C" fn(vm_ptr: i64, holder_class_id: i64,
    /// cp_idx: i64) -> i64`. Returns the target class's mirror `ObjectRef`,
    /// or `0` after publishing a pending exception.
    ///
    /// CP-indexed rather than class-id-indexed for the same reason
    /// [`Self::new_object_cp`] is: resolving the target can run a user
    /// `ClassLoader.loadClass`, which must not happen inside the compiler, so
    /// the *referencing* class id and the CP index are baked and resolution
    /// happens at run time on the executing thread.
    ///
    /// Re-consulted on every execution, like [`Self::ldc_string`] and unlike
    /// a baked immediate: a mirror is a heap object that a moving collector
    /// can relocate between two invocations of the same compiled body.
    ///
    /// `0` = not wired (hand-built test tables) → the backend refuses a
    /// class-`ldc` site and bails the compile, which is the pre-fix
    /// behaviour. Appended at the END of the struct so all prior golden
    /// offsets stay stable.
    pub ldc_class_cp: usize,

    /// `aastore` element-type check — the JVMS §6.5 *aastore* covariance rule
    /// and NOTHING else: no null check, no bounds check, no barrier, no store.
    ///
    /// `extern "C" fn(vm_ptr: i64, array_ptr: i64, val: i64) -> i64`. Returns
    /// `0` when the store is legal and the `i64::MIN` deopt sentinel when it is
    /// not, having stashed a real `ArrayStoreException` through the JIT_THREAD
    /// TLS. The sentinel is a *defined* return value on purpose: a `-> ()`
    /// helper leaves RAX undefined, so an `emit_post_invoke_exception_check`
    /// after it would be testing garbage.
    ///
    /// The x64 emitter lowers `aastore` inline (null check, bounds check, SATB
    /// pre-write barrier, store, card mark) and so never reaches
    /// [`Self::aastore`]; this is the one piece of that helper the inline path
    /// cannot do for itself, because the answer needs the class manager. On a
    /// refusal the caller must skip the store, the SATB pre-write barrier and
    /// the card mark.
    ///
    /// Required, not optional: a `0` slot would leave the inline lowering
    /// storing with no check, which is the heap-type-confusion defect
    /// (`Object[] a = new String[1]; a[0] = anInteger;` leaving an `Integer`
    /// inside a `String[]`) this slot exists to close. Appended at the END of
    /// the struct so all prior golden offsets stay stable.
    pub aastore_type_check: usize,

    /// Guarded inline `getfield` READ side — address of the GC's process-global
    /// `JIT_READ_BOUNDS` table (`gc/src/gen_heap.rs`), six consecutive
    /// `AtomicUsize` words in the same `[b0, e0, b1, e1, b2, e2]` layout as
    /// [`Self::region_bounds_addr`], so the emitted containment sequence is
    /// byte-identical and only the baked address differs.
    ///
    /// **Why a second table rather than reusing the first.**
    /// [`Self::region_bounds_addr`] is doing two jobs whose answers diverge.
    /// Its documented job is "is this address mapped, so a raw load cannot
    /// fault" — a READ question. Its load-bearing job since G1-2
    /// (`g1-audit.md` §8.1) is "may an inline reference STORE skip the
    /// collector's write barrier", and G1/ZGC answer that by leaving the table
    /// EMPTY: a JNI-pinned, CSet-excluded G1 region is reachable only through
    /// its remembered set, so an inline store that skips
    /// `post_write_barrier_rset` loses that edge. One table, two questions,
    /// opposite answers — filling it to fix loads would silently unblock the
    /// stores it exists to block.
    ///
    /// **Who publishes.** `GenerationalHeap` (its three arenas, refreshed at
    /// every GC start/end) and `G1Collector` (its single contiguous `Box`
    /// arena, `[arena_base, arena_end)`, published once at construction and
    /// cleared on drop — G1's N regions are carved from ONE allocation, so
    /// three slots are more than the one it needs). **ZGC deliberately does
    /// NOT publish**, which is what keeps this table from landing ahead of the
    /// ZGC JIT load barrier: a compact reference slot there holds
    /// `Z_COLORED_TAG | colour | offset`, not a pointer, and inlining its load
    /// is the use-after-free `feature-designs/zgc-jit-load-barrier.md` exists
    /// to stop. Not publishing is a strictly stronger discharge of that
    /// obligation than a per-field-kind gate would be, and it costs G1 nothing:
    /// the measured containment-failure split there is 100% reference / 0%
    /// primitive.
    ///
    /// `0` = not wired (hand-built test tables) → the READ path keeps the
    /// checked `jit_getfield` helper, which is the pre-fix behaviour. Appended
    /// at the END of the struct so all prior golden offsets stay stable.
    pub read_bounds_addr: usize,
    /// Compiled local exception handlers — address of
    /// `extern "C" fn(vm_ptr: i64, site_ptr: i64, out_exc: *mut i64) -> i64`
    /// (`vm/src/jit/helpers.rs::jit_local_handler_lookup`).
    ///
    /// Called from a per-throwing-bci stub the instant a fallible site returns
    /// the `i64::MIN` sentinel. It answers which of THIS method's own
    /// exception-table entries takes the pending throwable — index into the
    /// site's compile-time candidate list, or `-1` for "this frame does not
    /// catch it" — and on a hit stores the throwable into the frame slot the
    /// handler's operand stack starts at.
    ///
    /// `0` = not wired (hand-built test tables, or the feature switched off)
    /// → the backend arms no local-handler stubs at all and every caught
    /// exception takes the reason-9 deopt / shared-sentinel route out of
    /// compiled code, which is the behaviour that predates the feature.
    /// Appended at the END of the struct so all prior golden offsets stay
    /// stable.
    pub local_handler_lookup: usize,

    /// `ldc <String>` — the interned literal named at `cp_idx` in
    /// `holder_class_id`'s constant pool.
    ///
    /// `extern "C" fn(vm_ptr: i64, holder_class_id: i64, cp_idx: i64) -> i64`.
    /// The CP-indexed twin of [`Self::ldc_class_cp`], and the SUPERSEDER of
    /// [`Self::ldc_string`], which bakes the literal's UTF-8 bytes instead and
    /// therefore has no key to answer from: JVMS §5.4.3 resolves a
    /// constant-pool entry once and records the result, and the record is
    /// keyed `(class, cp index)`. The bytes form re-derived the answer on every
    /// execution — the string pool's `RwLock`, a hash of the literal's whole
    /// content and a `memcmp` — measured at 18.4 ns against HotSpot's 0.2 ns
    /// (`probes/LdcConstCostProbe.java`).
    ///
    /// `0` = not wired (hand-built test tables) → the backend refuses a
    /// string-`ldc` site and bails the compile, exactly as an unwired
    /// [`Self::ldc_class_cp`] makes it refuse a class-`ldc` site. Appended at
    /// the END of the struct so all prior golden offsets stay stable.
    pub ldc_string_cp: usize,
    /// FFM element READ fast path — `MemorySegment.getAtIndex`.
    ///
    /// Signature:
    /// `extern "C" fn(seg: i64, index: i64, layout_kind: i64, out: *mut i64) -> i64`,
    /// returning 1 when it handled the access (value written through `out`) and
    /// 0 to DECLINE, in which case the emitted code falls through to the
    /// unchanged native dispatch for the same site. Everything it declines
    /// therefore keeps today's behaviour, exceptions included.
    ///
    /// `layout_kind` is the call site's LAYOUT reference with the `FFM_KIND_*`
    /// code (a compile-time constant from the descriptor, which names the
    /// `ValueLayout` subtype) OR-ed into its low three bits. The helper
    /// declines for a layout the native has not read in host byte order and
    /// for an address the layout's alignment does not admit (round 12 wave 5).
    ///
    /// These accessors are per-ELEMENT, and through the ordinary dispatch funnel
    /// they measure ~1158 ns/element against ~0.8 ns for a `short[]` element.
    ///
    /// `0` = not wired (hand-built test tables) → no FFM fast path is emitted
    /// and every site keeps its native dispatch. Appended at the END so all
    /// prior golden offsets stay stable.
    pub ffm_segment_get: usize,
    /// FFM element WRITE fast path — `MemorySegment.setAtIndex`. The twin of
    /// [`Self::ffm_segment_get`], same 1-handled / 0-declined contract.
    ///
    /// Signature:
    /// `extern "C" fn(seg: i64, index: i64, layout_kind: i64, value: i64) -> i64`
    /// (`layout_kind` as for [`Self::ffm_segment_get`]).
    /// `value` carries raw bits: the integral kinds in their low bytes, float
    /// and double as `to_bits()`.
    pub ffm_segment_set: usize,

    // ── Reference-store barrier plan ────────────────────────────────────
    //
    // Three addresses that let compiled code inline the collector's OWN
    // early-outs instead of paying a call to discover them.
    //
    // The whole design is one property: each word below names a PREFIX of the
    // helper's control flow, and compiled code reads the same word the helper
    // itself reads. The JIT never reimplements a barrier — when a gate says
    // "there may be work" it calls the same helper it calls today, so no
    // collector's remembered-set contract moves into the emitter. What the JIT
    // gains is the right to skip the CALL when a gate proves the helper would
    // have returned immediately.
    //
    // This is deliberately NOT `region_bounds_addr`'s question. That table
    // asks "is the receiver in a published young region", which is a
    // GENERATIONAL question G1 and ZGC do not answer — so under the default
    // collector its emptiness routed every reference store to the helper and
    // still emitted six containment compares that could never pass. See the
    // field docs on `region_bounds_addr` and `read_bounds_addr` for that
    // history.
    /// Address of a `u8` that is **zero exactly when the SATB pre-write
    /// barrier is a no-op for every old value**.
    ///
    /// For ZGC this is `ZgcRealHeap::mark_active`; `satb_pre_barrier`'s entire
    /// body on a non-marking run is a relaxed load of it and a return.
    ///
    /// **Why an inline test of it is sound and not a race.** The flag is only
    /// ever ARMED inside a stop-the-world pause (`start_concurrent_mark` takes
    /// a `StopTheWorldToken` and arms it at step 3), so no mutator can sit
    /// between this test and its store while the flag turns on: every mutator
    /// is parked, and observes the armed flag when it resumes. Disarming is the
    /// safe direction — a mutator that skips the barrier after the mark phase
    /// ended has nothing to contribute to a completed snapshot.
    ///
    /// `0` = the collector does not publish one ⇒ compiled code must keep the
    /// pre-barrier, i.e. route the store to the full helper. Appended at the
    /// END of the struct so all prior golden offsets stay stable.
    pub ref_store_pre_gate: usize,
    /// Address of a `u8` that is **zero exactly when the post-write barrier is
    /// a no-op for every `(receiver, value)` pair**.
    ///
    /// For ZGC this is `ZgcRealHeap::has_old_objects`: `note_ref_store` returns
    /// on it before doing anything else, because with no old object in the heap
    /// there is no old-to-young edge to remember.
    ///
    /// `0` = not published ⇒ compiled code must always run the post barrier
    /// (call `write_barrier`).
    pub ref_store_post_gate: usize,
    /// Address of a `u8` `F` such that a receiver whose `GC_FLAGS_BYTE_OFFSET`
    /// byte is **unsigned-less-than `F`** provably needs no post barrier.
    ///
    /// That byte holds `gc_age` in bits 4..7 and the GC flags in bits 0..3, so
    /// with `F = promotion_age << 4` one unsigned byte compare is an EXACT test
    /// of `gc_age < promotion_age`: the flags nibble is at most 15, which
    /// cannot carry `age << 4` up to `(age + 1) << 4`. That is precisely ZGC's
    /// `note_ref_store_slow` early-out — a store into a young object needs no
    /// card, because a young cycle traces every young object anyway.
    ///
    /// Published as an address rather than baked as an immediate because the
    /// promotion age is dynamic; compiled code re-reads it at every store.
    ///
    /// `0` = the collector cannot express its post-barrier condition this way
    /// (Generational keys on `GC_FLAG_OLD_GEN`, a mask test rather than a
    /// floor; G1 keys on region state the emitter cannot see) ⇒ compiled code
    /// skips this test and falls back to `ref_store_post_gate` alone.
    pub ref_store_post_young_floor: usize,
    /// F-08 — address of the GC's `JIT_G1_BARRIER` table (`gc/src/gen_heap.rs`),
    /// baked as an immediate by the inline G1 post-write barrier.
    ///
    /// NOT a function pointer, and NOT a fourth spelling of
    /// [`Self::region_bounds_addr`]. That table's EMPTINESS under G1 is what
    /// closes defect G1-2, and it must keep answering "no inline reference
    /// store may skip the collector's barrier" there; this one answers the
    /// different question of what geometry an inline barrier needs in order to
    /// BE the barrier — arena base, arena length, region mask, and the F-05
    /// card table's base and shift. See `gc/src/gen_heap.rs::JitG1BarrierTable`.
    ///
    /// `0`, or a table whose `arena_len` word is zero, = no G1 collector has
    /// published → the emitter emits no inline barrier and every compiled
    /// reference store keeps the `putfield_object` helper it takes today.
    /// Appended at the END of the struct so all prior golden offsets stay
    /// stable.
    pub g1_barrier_addr: usize,
    /// F-08 — G1's post-write barrier, called from the inline arm when its two
    /// inline filters (same region, null store) both fail to prove there is
    /// nothing to remember.
    ///
    /// `extern "C" fn(vm_ptr: i64, obj_ptr: i64, val_ptr: i64)`.
    ///
    /// Distinct from [`Self::write_barrier`], which routes through
    /// `VmHeap::write_barrier` and carries a `debug_assert!` requiring an SATB
    /// pre-barrier on the same thread when a mark cycle is active. That
    /// assertion is right for a general store and wrong for this caller: the
    /// inline arm stores only when the field's OLD value is NULL, which is
    /// exactly the case `satb_pre_barrier` returns from immediately, so no
    /// pre-barrier is fired and none is owed. A dedicated entry point says that
    /// once, here, instead of weakening an assertion that protects every other
    /// caller.
    ///
    /// `0` = not wired (hand-built test tables) → no inline G1 barrier is
    /// emitted. Appended at the END of the struct so all prior golden offsets
    /// stay stable.
    pub g1_post_write_barrier: usize,
    /// The post-barrier SKIP MASK, as a VALUE — not an address like the three
    /// gate slots above it.
    ///
    /// A receiver none of whose `GC_FLAGS_BYTE_OFFSET` bits fall in this mask
    /// provably needs no post barrier, so compiled code can skip the call with
    /// a single `test r8, imm8`. `0` means the publisher uses
    /// `ref_store_post_young_floor` instead, or that nothing is published.
    ///
    /// **This is what let the GENERATIONAL collector publish a plan at all.**
    /// The floor is an unsigned compare on `(gc_age << 4) | gc_flags` and so
    /// can only say "young enough"; that collector's post barrier asks whether
    /// the receiver is in the OLD GENERATION, which is a flag bit
    /// (`GC_FLAG_OLD_GEN`). The two do not order — an object allocated straight
    /// into old gen has `gc_age == 0` and therefore a flags byte BELOW the
    /// age-zero floor — so a threshold would have told compiled code to skip
    /// the card on exactly the receivers that need one.
    ///
    /// A value rather than an address because which collector is running does
    /// not change after start-up, so the emitter bakes it as an immediate. The
    /// gate bytes stay addresses because they do change while the process runs.
    ///
    /// `0` = not wired (hand-built test tables). Appended at the END of the
    /// struct so all prior golden offsets stay stable.
    pub ref_store_post_skip_mask: usize,
    /// ZGC inline-TLAB announce helper — `extern "C" fn(vm_ptr, obj_ptr,
    /// size_bytes) -> i64`.
    ///
    /// Under ZGC with `jit_tlab_registration_required()` the collector must
    /// learn about every object the inline bump pointer carves (the page's
    /// start bit, allocate-black while marking). When the class's
    /// `tlab_post_init` work is otherwise a no-op, compiled code can call this
    /// narrower helper instead: it announces the object and returns `obj_ptr`
    /// unchanged, or `0` after publishing a pending exception.
    ///
    /// `0` = not wired: the emitter keeps calling `tlab_post_init`, which
    /// already announces the object itself. Logically it belongs beside
    /// `tlab_post_init`; it is appended at the END of the struct (ABI v13) so
    /// all prior golden offsets stay stable.
    pub zgc_note_tlab_object: usize,
    /// `multianewarray` of ANY arity - `extern "C" fn(vm_ptr, site, ndims,
    /// dims_ptr) -> i64`.
    ///
    /// The N-dimensional successor to `multianewarray_2d`, which could only
    /// ever describe two dimensions because four integer argument registers is
    /// all Win64 has and `(vm_ptr, site, dim1, dim2)` spends all four. This one
    /// spends the last register on a POINTER instead: `dims_ptr` addresses
    /// `ndims` consecutive `i64` dimension counts, outermost first, which the
    /// caller has written into its own frame. Arity then costs nothing.
    ///
    /// `multianewarray_2d` stays in the table -- the ABI is append-only -- and
    /// is no longer emitted; see the note on that field.
    ///
    /// Appended at the END of the struct (ABI v14) so all prior golden offsets
    /// stay stable.
    pub multianewarray_n: usize,
    /// Inline thin-lock `monitorenter` / `monitorexit` — byte offset of the
    /// `JitMonitorBlock` from `&JvmThread` (a displacement, like
    /// [`Self::shadow_stack_offset_in_thread`], not a pointer).
    ///
    /// The block is `#[repr(C)] { thin_owner: u64, lock_stack: *const
    /// JmxLockStack, .., held: *const LeaseBlock }`
    /// ([`JIT_MONITOR_BLOCK_THIN_OWNER_OFFSET`],
    /// [`JIT_MONITOR_BLOCK_LOCK_STACK_OFFSET`], [`JIT_MONITOR_BLOCK_HELD_OFFSET`]):
    /// the low 16 bits a thin lock by this thread writes into a mark word
    /// (`MARK_THIN_LOCKED | lease_slot << THIN_LOCK_OWNER_SHIFT`), or `u64::MAX`
    /// for "no inline path on this thread"; the owner-written JMX lock stack
    /// an inline acquisition must be recorded in (layout:
    /// [`JIT_LOCK_STACK_SEQ_OFFSET`] and its siblings); and the lease slot's
    /// `LeaseBlock` (VM `threading::monitor`): first the lessee's thin-lock
    /// count every outermost thin acquire / release keeps, then the one-entry
    /// inflated-monitor cache (`cratonvm_jit::runtime_lowering::LEASE_BLOCK_*`).
    /// The VM pins both layouts against these constants at compile time.
    ///
    /// `0` = not wired: every monitor site keeps its helper call, byte for byte.
    /// The VM publishes `0` when `CRATONVM_MONITOR_FASTPATH=0`, which is what
    /// makes that flag the kill switch for the inline path too. Appended at the
    /// END of the struct (ABI v15) so all prior golden offsets stay stable.
    pub monitor_block_offset_in_thread: usize,
    /// Compiled native-stack checks — byte offset of the `usize` stack-floor
    /// word from `&JvmThread` (a displacement, like
    /// [`Self::shadow_stack_offset_in_thread`], not a pointer). The VM writes
    /// the current OS thread's compiled-recursion floor there whenever it
    /// publishes the thread (`set_jit_thread`), so an inline-cache site can
    /// check `RSP` against it through the `JIT_THREAD` TLS mirror (`MOV RAX,
    /// <seg>:[mirror]; CMP RSP, [RAX + this]`) instead of a floor cached in
    /// its frame by a per-entry leaf call. A zero word never trips.
    ///
    /// `0` = not wired: every site keeps its frame-slot check, byte for byte.
    /// The VM publishes `0` under `CRATONVM_JIT_STACK_FLOOR_WORD=0`. Appended
    /// at the END of the struct (ABI v16, round 13 wave 4, lane mega3,
    /// proposal M13-3) so all prior golden offsets stay stable.
    pub jit_stack_floor_offset_in_thread: usize,
}

/// Offset of `thin_owner` (`u64`) inside the per-thread `JitMonitorBlock` that
/// [`JitRuntimeHelpers::monitor_block_offset_in_thread`] locates: the state and
/// owner bits of a thin lock this thread takes at recursion 0
/// (`MARK_THIN_LOCKED | slot << THIN_LOCK_OWNER_SHIFT`, where `slot` is the
/// thread's per-VM lock-slot LEASE, never its thread id), zero-extended.
/// `u64::MAX` there means "this thread takes no inline monitor path".
pub const JIT_MONITOR_BLOCK_THIN_OWNER_OFFSET: usize = 0;
/// Offset of `lock_stack` (`*const JmxLockStack`, never null once
/// `thin_owner` is not `u64::MAX`) inside the `JitMonitorBlock`.
pub const JIT_MONITOR_BLOCK_LOCK_STACK_OFFSET: usize = 8;
/// Offset of `held` (the lease's `LeaseBlock` address, never null once
/// `thin_owner` is not `u64::MAX`) inside the `JitMonitorBlock`. The block's
/// first word is the lessee's thin-lock count (`u32`), which an inline thin
/// acquire (NEUTRAL -> THIN_LOCKED) increments and an inline outermost release
/// (THIN_LOCKED at recursion 0 -> NEUTRAL) decrements with a plain `ADD` /
/// `SUB` after its winning CAS -- exactly what the helper path's
/// `note_acquired` / `note_owner_released` do; the lessee is its only writer
/// (an inflation uncounts in the VM's separate `stolen` word). A dying
/// thread's lease is given back only when the net count is 0. The rest of the
/// block is the inflated-monitor cache compiled code reads
/// (`cratonvm_jit::runtime_lowering::LEASE_BLOCK_*_OFFSET`).
pub const JIT_MONITOR_BLOCK_HELD_OFFSET: usize = 48;
/// `JmxLockStack::seq` (`u64`): single-writer seqlock, odd while the owner
/// writes. The owner's write is `seq += 1`, slot store, `top` store,
/// `seq += 1`.
pub const JIT_LOCK_STACK_SEQ_OFFSET: usize = 0;
/// `JmxLockStack::top` (`usize`): one past the highest used slot.
pub const JIT_LOCK_STACK_TOP_OFFSET: usize = 8;
/// `JmxLockStack::slots` (`[usize; JIT_LOCK_STACK_SLOTS]`): object addresses,
/// `0` = empty or retracted.
pub const JIT_LOCK_STACK_SLOTS_OFFSET: usize = 16;
/// Number of owner-written slots in a `JmxLockStack`.
pub const JIT_LOCK_STACK_SLOTS: usize = 8;
/// `JmxLockStack::spilled` (one byte, `bool`): non-zero while the mutexed
/// overflow list may be non-empty, when every publish must go there to keep
/// publish order — so the inline enter must not push a slot.
pub const JIT_LOCK_STACK_SPILLED_OFFSET: usize = 80;
/// Census counters in the `JitMonitorBlock` (`u64` each, owner-written): how
/// many times this thread's compiled monitor sites took the inline
/// `monitorenter` / `monitorexit`, and how many times they fell to the helper
/// call instead. Compiled code touches them only at sites compiled under
/// `CRATONVM_DBG_JITC` (a plain `ADD QWORD [thread + block + off], 1`); the VM
/// prints them under the same flag (`[monitor-inline census]`). Everywhere
/// else they stay zero and no emitted byte changes.
pub const JIT_MONITOR_BLOCK_INLINE_ENTERS_OFFSET: usize = 16;
/// See [`JIT_MONITOR_BLOCK_INLINE_ENTERS_OFFSET`].
pub const JIT_MONITOR_BLOCK_INLINE_EXITS_OFFSET: usize = 24;
/// See [`JIT_MONITOR_BLOCK_INLINE_ENTERS_OFFSET`].
pub const JIT_MONITOR_BLOCK_SLOW_ENTERS_OFFSET: usize = 32;
/// See [`JIT_MONITOR_BLOCK_INLINE_ENTERS_OFFSET`].
pub const JIT_MONITOR_BLOCK_SLOW_EXITS_OFFSET: usize = 40;

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
    (set_throw_bci,                  FieldKind::RequiredPtr),
    // Optional: a hand-built test helpers table leaves it 0, and the codegen
    // then emits no sentinel check — the pre-existing behaviour.
    (service_callee_deopt,           FieldKind::OptionalPtr),
    // Optional: a hand-built test helpers table leaves these 0, and the
    // backend then refuses a deferred (not-yet-loaded) `new`/`anewarray`
    // site and bails the compile — the pre-existing behaviour.
    (new_object_cp,                  FieldKind::OptionalPtr),
    (anewarray_object_cp,            FieldKind::OptionalPtr),
    // Optional: 0 makes `ir_lower` refuse a graph containing monitor ops.
    (monitor_enter,                  FieldKind::OptionalPtr),
    (monitor_exit,                   FieldKind::OptionalPtr),
    // Optional: 0 makes the single-pass backend refuse an `ldc <Class>` site
    // and bail the compile — the pre-fix behaviour.
    (ldc_class_cp,                   FieldKind::OptionalPtr),
    // Required: the `0x53` lowering is inline and calls this for the JVMS §6.5
    // covariance check; 0 would mean a reference store with no check at all.
    (aastore_type_check,             FieldKind::RequiredPtr),
    // NOT a pointer: address of the GC's JIT_READ_BOUNDS table, baked as an
    // immediate by the guarded inline getfield READ path. Deliberately a
    // DIFFERENT table from region_bounds_addr above -- see the field doc.
    (read_bounds_addr,               FieldKind::Offset),
    // Optional: 0 makes the single-pass backend arm no local-handler stubs, so
    // every caught exception keeps leaving compiled code — the pre-feature
    // behaviour.
    (local_handler_lookup,           FieldKind::OptionalPtr),
    // Optional: 0 makes both backends refuse a string-`ldc` site and bail the
    // compile, the same way an unwired `ldc_class_cp` does for `ldc <Class>`.
    (ldc_string_cp,                  FieldKind::OptionalPtr),
    (ffm_segment_get,                FieldKind::OptionalPtr),
    (ffm_segment_set,                FieldKind::OptionalPtr),
    // NOT pointers-to-code: addresses of collector-owned gate BYTES. `Offset`
    // (validated as "may be 0") rather than a required pointer, because 0 is
    // the meaningful value "this collector publishes no plan" — every emitter
    // arm then keeps the full-helper path it has today. The dangerous
    // direction is a WRONG non-zero, which would elide a barrier; that is the
    // publisher's obligation, and it writes the address of a real `'static`
    // gate byte or nothing at all.
    (ref_store_pre_gate,             FieldKind::Offset),
    (ref_store_post_gate,            FieldKind::Offset),
    (ref_store_post_young_floor,     FieldKind::Offset),
    // NOT a pointer: address of the GC's JIT_G1_BARRIER table, baked as an
    // immediate by the inline G1 post-write barrier. 0 = not wired.
    (g1_barrier_addr,                FieldKind::Offset),
    (g1_post_write_barrier,          FieldKind::OptionalPtr),
    // A VALUE (a flags mask), not a pointer: `Constant` in the ABI table and
    // deliberately not range-checked as an address here.
    (ref_store_post_skip_mask,       FieldKind::Offset),
    // v13: the ZGC inline-TLAB announce helper. Optional: 0 keeps every
    // inline allocation on its `tlab_post_init` call.
    (zgc_note_tlab_object,           FieldKind::OptionalPtr),
    // v14: the N-dimensional `multianewarray` helper, which supersedes
    // `multianewarray_2d`. Required, not optional: the emitter has no
    // fallback for a site it has already admitted -- `multianewarray_2d`
    // cannot serve one, which is the whole reason this slot exists.
    (multianewarray_n,               FieldKind::RequiredPtr),
    // v15: displacement of the per-thread `JitMonitorBlock` in `JvmThread`.
    // 0 = not wired: every monitor site keeps its helper call.
    (monitor_block_offset_in_thread, FieldKind::Offset),
    // v16: displacement of the native-stack floor word in `JvmThread`.
    // 0 = not wired: every inline-cache stack check keeps its frame slot.
    (jit_stack_floor_offset_in_thread, FieldKind::Offset),
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
    JitRuntimeHelpers::NUM_FIELDS == 79,
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
mod tests;
