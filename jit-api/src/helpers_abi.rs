// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Typed ABI description for [`JitRuntimeHelpers`].
//!
//! [`JitRuntimeHelpers`] is, and must remain, a `#[repr(C)]` table of bare
//! `usize` words: the JIT bakes each slot's **byte offset** into generated RWX
//! machine code (`CALL [helpers + disp32]`, `MOV reg, [helpers + disp32]`), so
//! the field order, field count, and per-field width are a frozen binary
//! contract. Changing a field's Rust *type* would change nothing about the
//! emitted code but would break the `size_of::<JitRuntimeHelpers>() / 8`
//! identity every offset assertion in this crate depends on.
//!
//! This module adds — **without touching the layout** — the information the
//! raw `usize` throws away:
//!
//! * one `HelperFn*` type alias per callable slot, carrying the real C
//!   signature (arity, integer widths, return type, `extern "C"`);
//! * [`HELPER_FIELDS`], a machine-readable descriptor table (name, byte
//!   offset from `core::mem::offset_of!`, kind, required-ness) covering every
//!   slot, so external tooling and the VM can reason about the table without
//!   re-deriving offsets by hand;
//! * typed, null-checked accessors (`<field>_fn`) that return
//!   `Option<HelperFn*>` instead of a naked integer;
//! * [`JitRuntimeHelpers::validate_with`], a version-checked validator that
//!   names the first missing required slot.
//!
//! # ABI contract
//!
//! 1. `#[repr(C)]` is mandatory. `repr(Rust)` may reorder fields; every baked
//!    `disp32` would then address the wrong helper with no compile error.
//! 2. Every field is `usize` (8 bytes; the JIT is x86-64 only). The byte
//!    offset of field *N* is exactly `N * 8`.
//! 3. Fields are **append-only**. Never reorder, never remove, never change a
//!    field's type. New fields go at the end.
//! 4. Any change to points 1-3 — including adding a field — must bump
//!    [`JIT_HELPERS_ABI_VERSION`], because a producer built against the old
//!    layout and a consumer built against the new one cannot be told apart by
//!    size alone once a field is appended.
//! 5. Nullability is per field and is recorded in [`HELPER_FIELDS`]:
//!    * `required == true` — the backend emits an unconditional absolute
//!      `CALL` to this slot. Zero means a `CALL 0` and an immediate fault.
//!    * `required == false` — zero is the documented "not wired" sentinel;
//!      the backend checks for it and falls back to a slower path (or emits
//!      nothing at all).
//!
//! # Relationship to `helper_fields!` in the crate root
//!
//! The crate root already carries a `helper_fields!` macro list that drives
//! [`JitRuntimeHelpers::validate`] / `null_pointers`. That list is the
//! *validation* source of truth and is classified with [`crate::FieldKind`];
//! [`HELPER_FIELDS`] here is the *ABI-description* source of truth and is
//! classified with [`HelperKind`], which additionally distinguishes a byte
//! offset from a baked absolute address. The two are cross-checked against
//! each other by `helper_fields_agree_with_crate_root` in this module's tests,
//! so neither can drift.

use core::ffi::c_void;

use crate::JitRuntimeHelpers;

/// ABI revision of the [`JitRuntimeHelpers`] layout.
///
/// Bump this whenever the struct's shape changes in any way — a new appended
/// field, a removed field, a reordering, a width change. Producers (the VM's
/// `build_helpers`) and consumers (the JIT backends) that disagree on this
/// number disagree on where the helpers live.
///
/// `1` is the revision of the 58-field, 464-byte table shipped today.
pub const JIT_HELPERS_ABI_VERSION: u32 = 1;

/// Size in bytes of the helper table under [`JIT_HELPERS_ABI_VERSION`].
///
/// Pinned literally by a `const _: () = assert!(...)` below so an accidental
/// layout change fails the build rather than silently re-basing every offset.
pub const JIT_HELPERS_ABI_SIZE: usize = core::mem::size_of::<JitRuntimeHelpers>();

/// Alignment of the helper table. `usize` alignment on x86-64, i.e. 8.
pub const JIT_HELPERS_ABI_ALIGN: usize = core::mem::align_of::<JitRuntimeHelpers>();

/// Byte stride between consecutive slots. Every field is a `usize`, so this is
/// also the width of a single slot and `offset(N) == N * HELPER_FIELD_STRIDE`.
pub const HELPER_FIELD_STRIDE: usize = core::mem::size_of::<usize>();

/// Upper bound accepted by [`JitRuntimeHelpers::validate_with`] for a
/// [`HelperKind::Offset`] slot.
///
/// The offset slots are displacements *within* a `JvmThread` (or within an
/// object header). One mebibyte is orders of magnitude above any real value
/// and orders of magnitude below a plausible absolute address, so a value past
/// this bound means a pointer was stored where an offset was expected — the
/// exact confusion the raw-`usize` table used to permit silently.
pub const MAX_PLAUSIBLE_OFFSET: usize = 1 << 20;

/// What a [`JitRuntimeHelpers`] slot actually holds.
///
/// The struct stores every slot as a bare `usize`; this is the distinction the
/// integer type erases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelperKind {
    /// A callable entry point. The backend emits an absolute `CALL` to it.
    /// Its C signature is given by the matching `HelperFn*` alias and it is
    /// reachable from Rust through the matching `<field>_fn` accessor.
    Function,
    /// A byte offset (displacement) baked as a `disp32` immediate — e.g. the
    /// TLAB cursor's offset from `&JvmThread`. Never dereferenced as a
    /// pointer; zero can be a legitimate value.
    Offset,
    /// A baked absolute address or bound that is **not** callable — the GC's
    /// region-bounds table, the safepoint flag byte, the card-table base.
    /// Loaded as data (`MOV r64, imm64`), never `CALL`ed. Zero means the
    /// mechanism is disabled for this collector/configuration.
    Constant,
}

/// Machine-readable description of one [`JitRuntimeHelpers`] slot.
///
/// `offset` is produced by `core::mem::offset_of!`, so it cannot drift from
/// the struct; the const assertions below additionally pin it to
/// `index * HELPER_FIELD_STRIDE`, which is what the JIT bakes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelperFieldDesc {
    /// Rust field name, exactly as declared on [`JitRuntimeHelpers`].
    pub name: &'static str,
    /// Byte offset of the slot from the base of the table.
    pub offset: usize,
    /// Whether the slot is a call target, an offset, or a baked address.
    pub kind: HelperKind,
    /// `true` when zero is invalid: the backend `CALL`s this slot
    /// unconditionally. `false` when zero is the "not wired" sentinel.
    pub required: bool,
}

// ---------------------------------------------------------------------
// Callable slots: signature aliases + typed accessors.
//
// Each row is `Alias, field, accessor, (args) -> ret;`. The argument and
// return types are those of the `extern "C"` function whose address
// `vm/src/jit/helpers.rs::build_helpers` stores in the slot; the parameter
// names are given in the comment above each group.
//
// `usize`/`i64` widths matter: the JIT loads arguments into the platform
// ARG_REGS as full 64-bit values, and a narrower Rust type here would be a
// silent ABI lie even though the emitted code is unchanged.
// ---------------------------------------------------------------------
macro_rules! helper_fn_slots {
    ( $( $alias:ident, $field:ident, $getter:ident, ( $($arg:ty),* ) -> $ret:ty ; )* ) => {
        $(
            #[doc = concat!(
                "C signature of the `", stringify!($field),
                "` slot of [`JitRuntimeHelpers`]."
            )]
            ///
            /// Declared `unsafe` because the value reached the table as a raw
            /// address: nothing in the type system ties it to the function the
            /// VM intended, and every one of these helpers dereferences at
            /// least one JIT-supplied raw pointer.
            pub type $alias = unsafe extern "C" fn( $($arg),* ) -> $ret;
        )*

        impl JitRuntimeHelpers {
            $(
                #[doc = concat!(
                    "Typed view of the `", stringify!($field), "` slot.\n\n",
                    "Returns [`None`] when the slot is zero (unwired), so a \
                     null helper can no longer be `CALL`ed by accident."
                )]
                ///
                /// # Safety of the returned pointer
                ///
                /// The returned value is safe to *hold*; calling it is
                /// `unsafe` and carries the whole JIT-boundary contract:
                ///
                /// * **Precondition** — the slot must have been populated by
                ///   `vm/src/jit/helpers.rs::build_helpers` (or, in the
                ///   `jit/tests` tables, by a stub with this exact
                ///   signature). A table filled from any other source may hold
                ///   an address that is not a function at all.
                /// * **ABI** — the callee is `extern "C"` and follows the
                ///   platform C calling convention (SysV or Win64); integer
                ///   arguments are passed as full 64-bit words even where the
                ///   Java-level value is narrower, and floating-point helpers
                ///   use XMM0.. on both ABIs.
                /// * **Ownership** — pointer-typed arguments (`vm_ptr`,
                ///   `obj_ptr`, `info_ptr`, `args_ptr`, …) are *borrowed* for
                ///   the duration of the call only. The helper never takes
                ///   ownership and never frees them; the caller must keep them
                ///   live and unaliased across the call.
                /// * **Lifetime** — the address is valid for as long as the
                ///   VM image that produced it is loaded, which for the
                ///   in-process VM is the life of the process. It is *not*
                ///   tied to the lifetime of the `&self` borrow, so the
                ///   returned function pointer may outlive this table.
                /// * **Thread** — helpers must be called from a thread that
                ///   is attached to the VM (they touch `JIT_SIGNALS` and other
                ///   thread-locals, and most may enter a GC safepoint). They
                ///   are not signal-safe and must not be called re-entrantly
                ///   from inside a GC callback.
                /// * **Managed state** — a helper may trigger a collection and
                ///   therefore may move managed objects. Raw object addresses
                ///   held across a call must be re-read, never cached.
                pub fn $getter(&self) -> Option<$alias> {
                    let raw = self.$field;
                    if raw == 0 {
                        return None;
                    }
                    // SAFETY: `raw` is non-zero and, per the table's
                    // construction contract (`build_helpers` stores
                    // `f as *const () as usize` for exactly the function whose
                    // signature this alias mirrors), is the entry point of an
                    // `extern "C"` function with this signature. `usize` and a
                    // function pointer have the same size and validity
                    // requirement modulo non-null, which is checked above, so
                    // the transmute is well defined. Constructing the pointer
                    // performs no call and cannot observe VM state; every
                    // obligation listed under "Safety of the returned pointer"
                    // above is discharged by the caller at the call site, not
                    // here.
                    Some(unsafe { core::mem::transmute::<usize, $alias>(raw) })
                }
            )*

            /// Number of callable slots ([`HelperKind::Function`]) in the
            /// table, derived from the alias list above rather than counted by
            /// hand. Cross-checked against [`HELPER_FIELDS`] by a const
            /// assertion.
            pub const NUM_HELPER_FN_FIELDS: usize =
                [ $( helper_fn_slots!(@unit $field) ),* ].len();
        }
    };
    (@unit $field:ident) => { () };
}

helper_fn_slots! {
    // Allocation. (vm_ptr, atype, length) / (vm_ptr, class_id, num_fields) /
    // (vm_ptr, component_class_id, length).
    HelperFnNewarray, newarray, newarray_fn, (i64, i64, i64) -> i64;
    HelperFnNewObject, new_object, new_object_fn, (i64, i64, i64) -> i64;
    HelperFnAnewarrayObject, anewarray_object, anewarray_object_fn, (i64, i64, i64) -> i64;

    // Array access. Loads: (array_ptr, index). Primitive stores:
    // (array_ptr, index, val). `aastore` additionally takes `vm_ptr` first
    // because a reference store runs the write barrier.
    HelperFnBaload, baload, baload_fn, (i64, i64) -> i64;
    HelperFnBastore, bastore, bastore_fn, (i64, i64, i64) -> ();
    HelperFnIaload, iaload, iaload_fn, (i64, i64) -> i64;
    HelperFnIastore, iastore, iastore_fn, (i64, i64, i64) -> ();
    HelperFnAaload, aaload, aaload_fn, (i64, i64) -> i64;
    HelperFnAastore, aastore, aastore_fn, (i64, i64, i64, i64) -> ();
    HelperFnMultianewarray2d, multianewarray_2d, multianewarray_2d_fn,
        (i64, i64, i64, i64) -> i64;
    HelperFnArraylength, arraylength, arraylength_fn, (i64) -> i64;

    // Instance fields. `getfield` takes (vm_ptr, obj_ptr, field_index); the
    // primitive `putfield_*` helpers take (obj_ptr, field_index, val) with NO
    // vm_ptr — only the reference store needs the VM for its barrier.
    HelperFnGetfield, getfield, getfield_fn, (i64, i64, i64) -> i64;
    HelperFnPutfieldInt, putfield_int, putfield_int_fn, (i64, i64, i64) -> ();
    HelperFnPutfieldLong, putfield_long, putfield_long_fn, (i64, i64, i64) -> ();
    HelperFnPutfieldFloat, putfield_float, putfield_float_fn, (i64, i64, i64) -> ();
    HelperFnPutfieldDouble, putfield_double, putfield_double_fn, (i64, i64, i64) -> ();
    HelperFnPutfieldObject, putfield_object, putfield_object_fn, (i64, i64, i64, i64) -> ();

    // Statics. (vm_ptr, class_id, field_index[, val]); the `putstatic_*`
    // helpers return the `i64::MIN` deopt sentinel when class init must run.
    HelperFnGetstatic, getstatic, getstatic_fn, (i64, i64, i64) -> i64;
    HelperFnPutstaticInt, putstatic_int, putstatic_int_fn, (i64, i64, i64, i64) -> i64;
    HelperFnPutstaticLong, putstatic_long, putstatic_long_fn, (i64, i64, i64, i64) -> i64;
    HelperFnPutstaticFloat, putstatic_float, putstatic_float_fn, (i64, i64, i64, i64) -> i64;
    HelperFnPutstaticDouble, putstatic_double, putstatic_double_fn, (i64, i64, i64, i64) -> i64;
    HelperFnPutstaticObject, putstatic_object, putstatic_object_fn, (i64, i64, i64, i64) -> i64;

    // Type checks. (vm_ptr, obj_ptr, class_name_ptr, class_name_len) — the
    // name bytes are owned by the CompiledMethod and outlive the call.
    HelperFnCheckcast, checkcast, checkcast_fn, (i64, i64, *const u8, i64) -> i64;
    HelperFnInstanceofCheck, instanceof_check, instanceof_check_fn,
        (i64, i64, *const u8, i64) -> i64;

    // Direct throws. AIOOBE: (index, length, array_ptr, bytecode_pc).
    // ArithmeticException takes no arguments; both return the deopt sentinel.
    HelperFnThrowAioobe, throw_aioobe, throw_aioobe_fn, (i64, i64, i64, i64) -> i64;
    HelperFnThrowArithmetic, throw_arithmetic, throw_arithmetic_fn, () -> i64;

    // Invocation. (vm_ptr, info_ptr, args_ptr, num_args) plus, for the
    // monomorphic/polymorphic inline-cache variant, (mic_ptr, pic_ptr).
    HelperFnInvokeDispatch, invoke_dispatch, invoke_dispatch_fn, (i64, i64, i64, i64) -> i64;
    HelperFnInvokeVirtualMic, invoke_virtual_mic, invoke_virtual_mic_fn,
        (i64, i64, i64, i64, i64, i64) -> i64;

    // Scalar lambda adapter for TDigest numeric kernels.
    // (vm_ptr, proxy_raw, index).
    HelperFnLambdaIntToDouble, lambda_int_to_double, lambda_int_to_double_fn,
        (i64, i64, i64) -> i64;

    // GC barriers. Post-store: (vm_ptr, obj_ptr, val_ptr). SATB pre-write:
    // (vm_ptr, old_ref).
    HelperFnWriteBarrier, write_barrier, write_barrier_fn, (i64, i64, i64) -> ();
    HelperFnSatbPreWriteBarrier, satb_pre_write_barrier, satb_pre_write_barrier_fn,
        (i64, i64) -> ();

    // Speculation failure. (vm_ptr, reason, bci) -> deopt action code.
    HelperFnUncommonTrap, uncommon_trap, uncommon_trap_fn, (i64, i64, i64) -> i64;

    // Math.fma — the only slots whose arguments travel in XMM registers.
    HelperFnMathFmaDouble, math_fma_double, math_fma_double_fn, (f64, f64, f64) -> f64;
    HelperFnMathFmaFloat, math_fma_float, math_fma_float_fn, (f32, f32, f32) -> f32;

    // Inline TLAB allocation. `get_current_thread` returns a `*mut JvmThread`,
    // opaque to this crate (the vm crate owns the type), so it is typed
    // `*mut c_void` here — the width and nullability are what the ABI needs.
    // `tlab_post_init` takes (vm_ptr, obj_ptr, class_id, num_fields).
    HelperFnGetCurrentThread, get_current_thread, get_current_thread_fn, () -> *mut c_void;
    HelperFnTlabPostInit, tlab_post_init, tlab_post_init_fn, (i64, i64, i64, i64) -> i64;

    // Precise oop maps — (rbp). Called once from the JIT prologue.
    HelperFnFrameRecord, frame_record, frame_record_fn, (usize) -> ();

    // athrow lowering — (exc_ptr, bci) -> deopt sentinel. NOTE the second
    // argument: the crate-root field doc still describes the older one-argument
    // shape, but the backend loads ARG_REGS[1] with the throw site's bci.
    HelperFnThrowException, throw_exception, throw_exception_fn, (i64, i64) -> i64;

    // JEP 358 helpful-NPE inline stubs — (npe_action code).
    HelperFnJitNpeWithAction, jit_npe_with_action, jit_npe_with_action_fn, (i64) -> ();

    // `i64::MIN` disambiguation for J/D returns — no arguments, returns 1 when
    // a genuine exception/deopt is pending.
    HelperFnDispatchThrew, dispatch_threw, dispatch_threw_fn, () -> i64;

    // IR FP tier — fmod-style remainders, operands and result in XMM0/XMM1.
    HelperFnJitFrem, jit_frem, jit_frem_fn, (f32, f32) -> f32;
    HelperFnJitDrem, jit_drem, jit_drem_fn, (f64, f64) -> f64;

    // Native-stack headroom guard for direct self-recursive calls — (vm_ptr).
    HelperFnSelfCallStackGuard, self_call_stack_guard, self_call_stack_guard_fn,
        (i64) -> i64;

    // Leaf floor query for the inline self-recursion check. No arguments.
    // The accessor keeps the mechanical `<field>_fn` naming rule, which is why
    // it reads `native_stack_floor_fn_fn` — the field itself already ends in
    // `_fn` because it sits next to the non-callable `region_bounds_addr`.
    HelperFnNativeStackFloor, native_stack_floor_fn, native_stack_floor_fn_fn, () -> i64;

    // Interned string materialization for a compiled `ldc` — (vm_ptr,
    // utf8_ptr, utf8_len). `len` is a `usize`, not an `i64`.
    HelperFnLdcString, ldc_string, ldc_string_fn, (i64, *const u8, usize) -> i64;

    // Cooperative safepoint poll slow path. Takes NO arguments — the crate-root
    // field doc still mentions a `vm_ptr`, which the implementation does not
    // read (it resolves the process VM itself).
    HelperFnSafepointSlowPath, safepoint_slow_path, safepoint_slow_path_fn, () -> ();

    // Stamp this compiled method's own throw-site bci onto a pending signal.
    HelperFnSetThrowBci, set_throw_bci, set_throw_bci_fn, (i64) -> ();

    // Service a callee's deopt sentinel at an inline (MIC/PIC) call site —
    // (vm_ptr, info_ptr, args_ptr, num_args).
    HelperFnServiceCalleeDeopt, service_callee_deopt, service_callee_deopt_fn,
        (i64, i64, i64, i64) -> i64;
}

// ---------------------------------------------------------------------
// The descriptor table.
//
// Rows are in DECLARATION ORDER, which for a `#[repr(C)]` all-`usize` struct
// is also byte-offset order. Do not reorder rows; do not insert a row in the
// middle. Append only, and bump `JIT_HELPERS_ABI_VERSION` when you do.
//
// `required` is `true` only for slots the backend `CALL`s unconditionally.
// It intentionally matches `FieldKind::RequiredPtr` in the crate root, and
// the `helper_fields_agree_with_crate_root` test enforces that.
// ---------------------------------------------------------------------
macro_rules! helper_field_table {
    ( $( ($field:ident, $kind:ident, $required:expr) ),* $(,)? ) => {
        /// Machine-readable ABI description of every [`JitRuntimeHelpers`]
        /// slot, in byte-offset order.
        ///
        /// Offsets come from `core::mem::offset_of!`, so this table cannot
        /// drift from the struct; the const assertions below pin them to
        /// `index * HELPER_FIELD_STRIDE`, which is the value the JIT bakes
        /// into generated code.
        pub const HELPER_FIELDS: &[HelperFieldDesc] = &[
            $(
                HelperFieldDesc {
                    name: stringify!($field),
                    offset: core::mem::offset_of!(JitRuntimeHelpers, $field),
                    kind: HelperKind::$kind,
                    required: $required,
                },
            )*
        ];

        /// Number of described slots. Equals
        /// [`JitRuntimeHelpers::NUM_FIELDS`] by const assertion.
        pub const NUM_HELPER_FIELDS: usize = HELPER_FIELDS.len();
    };
}

helper_field_table! {
    (newarray,                       Function, true),
    (new_object,                     Function, true),
    (anewarray_object,               Function, true),
    (baload,                         Function, true),
    (bastore,                        Function, true),
    (iaload,                         Function, true),
    (iastore,                        Function, true),
    (aaload,                         Function, true),
    (aastore,                        Function, true),
    (multianewarray_2d,              Function, true),
    (arraylength,                    Function, true),
    (getfield,                       Function, true),
    (putfield_int,                   Function, true),
    (putfield_long,                  Function, true),
    (putfield_float,                 Function, true),
    (putfield_double,                Function, true),
    (putfield_object,                Function, true),
    (getstatic,                      Function, true),
    (putstatic_int,                  Function, true),
    (putstatic_long,                 Function, true),
    (putstatic_float,                Function, true),
    (putstatic_double,               Function, true),
    (putstatic_object,               Function, true),
    (checkcast,                      Function, true),
    (instanceof_check,               Function, true),
    (throw_aioobe,                   Function, true),
    (throw_arithmetic,               Function, true),
    (invoke_dispatch,                Function, true),
    (invoke_virtual_mic,             Function, true),
    (lambda_int_to_double,           Function, true),
    (write_barrier,                  Function, true),
    (satb_pre_write_barrier,         Function, true),
    (uncommon_trap,                  Function, true),
    (math_fma_double,                Function, true),
    (math_fma_float,                 Function, true),
    // Displacements within `JvmThread` / the object header.
    (tlab_cursor_offset_in_thread,   Offset,   false),
    (tlab_end_offset_in_thread,      Offset,   false),
    (class_id_offset_in_obj,         Offset,   false),
    // Optional: 0 => the backend falls back to the helper-call path.
    (get_current_thread,             Function, false),
    (tlab_post_init,                 Function, false),
    (frame_record,                   Function, false),
    (shadow_stack_offset_in_thread,  Offset,   false),
    (throw_exception,                Function, true),
    (jit_npe_with_action,            Function, true),
    (dispatch_threw,                 Function, true),
    (jit_frem,                       Function, true),
    (jit_drem,                       Function, true),
    (self_call_stack_guard,          Function, false),
    // Baked absolute address of the GC's JIT_REGION_BOUNDS table, not callable.
    (region_bounds_addr,             Constant, false),
    (native_stack_floor_fn,          Function, false),
    (ldc_string,                     Function, true),
    // Baked absolute address of the STW-requested flag byte, not callable.
    (safepoint_flag_addr,            Constant, false),
    (safepoint_slow_path,            Function, false),
    // Generational inline-card metadata: all zero under G1/ZGC.
    (jit_card_table_addr,            Constant, false),
    (jit_card_old_base,              Constant, false),
    (jit_card_old_end,               Constant, false),
    (set_throw_bci,                  Function, true),
    (service_callee_deopt,           Function, false),
}

// ---------------------------------------------------------------------
// Compile-time ABI pins.
//
// These use `const _: () = ...` items rather than inline `const { ... }`
// blocks to match the style of the crate root (workspace MSRV is 1.80, but
// the existing assertions were written for 1.77 and there is no reason to
// diverge).
// ---------------------------------------------------------------------

// The descriptor table must cover the whole struct — no field may be omitted.
const _: () = assert!(
    NUM_HELPER_FIELDS == JitRuntimeHelpers::NUM_FIELDS,
    "HELPER_FIELDS is missing a JitRuntimeHelpers field — every slot must be \
     described, add the new field to helper_field_table! in helpers_abi.rs",
);

// Pin the literal count so a *removal* also has to touch this line.
const _: () = assert!(
    NUM_HELPER_FIELDS == 58,
    "JitRuntimeHelpers field count changed — bump JIT_HELPERS_ABI_VERSION, the \
     literal here, and the size literal below",
);

// Pin the literal size and alignment. The JIT bakes `disp32` offsets derived
// from this layout into RWX memory; a silent change here is a wild call.
const _: () = assert!(
    JIT_HELPERS_ABI_SIZE == 464,
    "JitRuntimeHelpers size changed (expected 58 * 8 = 464) — the JIT's baked \
     helper offsets are now wrong; bump JIT_HELPERS_ABI_VERSION deliberately",
);
const _: () = assert!(
    JIT_HELPERS_ABI_ALIGN == 8,
    "JitRuntimeHelpers alignment changed — the table is loaded as an array of \
     machine words by generated code",
);
const _: () = assert!(
    JIT_HELPERS_ABI_SIZE == NUM_HELPER_FIELDS * HELPER_FIELD_STRIDE,
    "JitRuntimeHelpers gained padding or a non-usize field — the `offset(N) == \
     N * 8` identity the JIT relies on no longer holds",
);

// Every descriptor offset equals both `offset_of!` (by construction) and the
// sequential `index * 8` the JIT computes. Checking the second form catches a
// field reorder, which `offset_of!` alone would silently follow.
const _: () = {
    let mut i = 0;
    while i < NUM_HELPER_FIELDS {
        assert!(
            HELPER_FIELDS[i].offset == i * HELPER_FIELD_STRIDE,
            "HELPER_FIELDS row is not at its sequential ABI offset — a field \
             was reordered or the table rows are out of declaration order",
        );
        i += 1;
    }
};

// The callable-slot count derived from the signature-alias list must equal the
// number of rows the descriptor table marks as `Function`. This is the seam
// where a new field could be added to one list and forgotten in the other.
const _: () = {
    let mut i = 0;
    let mut fns = 0;
    let mut required = 0;
    while i < NUM_HELPER_FIELDS {
        if matches!(HELPER_FIELDS[i].kind, HelperKind::Function) {
            fns += 1;
        }
        if HELPER_FIELDS[i].required {
            required += 1;
        }
        i += 1;
    }
    assert!(
        fns == JitRuntimeHelpers::NUM_HELPER_FN_FIELDS,
        "a callable slot is described in HELPER_FIELDS but has no signature \
         alias (or vice versa) — add it to helper_fn_slots! in helpers_abi.rs",
    );
    assert!(
        required == 42,
        "the required-slot count changed — a helper was promoted or demoted; \
         confirm the backend really does (not) CALL it unconditionally",
    );
};

// A non-callable slot must never be marked required: `required` exists to
// reject a zero that would become `CALL 0`, and nothing ever CALLs an offset.
const _: () = {
    let mut i = 0;
    while i < NUM_HELPER_FIELDS {
        if HELPER_FIELDS[i].required {
            assert!(
                matches!(HELPER_FIELDS[i].kind, HelperKind::Function),
                "a non-callable slot is marked required — offsets and baked \
                 addresses are legitimately zero",
            );
        }
        i += 1;
    }
};

/// Why a [`JitRuntimeHelpers`] table was rejected by
/// [`JitRuntimeHelpers::validate_with`].
///
/// Carries the offending field name rather than an index so the message is
/// actionable without cross-referencing the ABI table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelperAbiError {
    /// A slot the backend `CALL`s unconditionally is zero. Baking this address
    /// would emit a `CALL 0`.
    MissingRequired(&'static str),
    /// The caller was built against a different revision of the layout.
    VersionMismatch {
        /// Revision this build of `jit-api` implements.
        expected: u32,
        /// Revision the caller declared.
        found: u32,
    },
    /// `size_of::<JitRuntimeHelpers>()` is not the pinned ABI size. Only
    /// reachable if the const assertions above were weakened.
    SizeMismatch {
        /// Pinned [`JIT_HELPERS_ABI_SIZE`].
        expected: usize,
        /// Size observed at runtime.
        found: usize,
    },
    /// A [`HelperKind::Offset`] slot holds a value far too large to be a
    /// displacement — almost certainly a pointer stored in the wrong slot.
    ImplausibleOffset {
        /// Field name.
        name: &'static str,
        /// The rejected value.
        value: usize,
    },
}

impl core::fmt::Display for HelperAbiError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            HelperAbiError::MissingRequired(name) => write!(
                f,
                "JIT helper table is missing required helper `{}` (slot is 0; \
                 the backend would emit a CALL to a null address)",
                name
            ),
            HelperAbiError::VersionMismatch { expected, found } => write!(
                f,
                "JIT helper ABI version mismatch: this build implements v{}, \
                 caller declared v{}",
                expected, found
            ),
            HelperAbiError::SizeMismatch { expected, found } => write!(
                f,
                "JIT helper table size mismatch: expected {} bytes, found {}",
                expected, found
            ),
            HelperAbiError::ImplausibleOffset { name, value } => write!(
                f,
                "JIT helper offset `{}` = {:#x} is too large to be a byte \
                 offset (limit {:#x}); a pointer may have been stored in an \
                 offset slot",
                name, value, MAX_PLAUSIBLE_OFFSET
            ),
        }
    }
}

impl std::error::Error for HelperAbiError {}

impl JitRuntimeHelpers {
    /// View the table as its raw machine words, in ABI order.
    ///
    /// `words[i]` is the slot described by `HELPER_FIELDS[i]`, i.e. the value
    /// generated code reads at `[helpers + i * 8]`. This is the reinterpretation
    /// the JIT itself performs, made explicit and bounds-checked.
    pub fn as_words(&self) -> &[usize; JitRuntimeHelpers::NUM_FIELDS] {
        // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a
        // `usize`, so its layout is exactly `[usize; NUM_FIELDS]` — pinned by
        // the `JIT_HELPERS_ABI_SIZE == NUM_HELPER_FIELDS * HELPER_FIELD_STRIDE`
        // const assertion above, which rules out both padding and a
        // differently-sized field. Alignment is identical (both `align_of::<
        // usize>()`), the target array type has no validity requirement a
        // `usize` field could violate, and the returned reference borrows
        // `self`, so no aliasing or lifetime obligation is introduced.
        unsafe { &*(self as *const JitRuntimeHelpers as *const [usize; Self::NUM_FIELDS]) }
    }

    /// Raw value of the slot described by `desc`.
    ///
    /// Reads by *offset*, exactly as generated code does, so a descriptor that
    /// disagreed with the struct would be caught here rather than silently
    /// reading a neighbouring field.
    pub fn word_at(&self, desc: &HelperFieldDesc) -> usize {
        self.as_words()[desc.offset / HELPER_FIELD_STRIDE]
    }

    /// Validate this table against a caller-supplied ABI revision.
    ///
    /// The struct deliberately carries **no** version or size word — adding one
    /// would shift every subsequent field and invalidate every offset the JIT
    /// has baked into RWX memory. The revision therefore travels out of band:
    /// the producer and the consumer each compile against
    /// [`JIT_HELPERS_ABI_VERSION`] and the consumer passes what it was built
    /// with.
    ///
    /// Checks, in order:
    ///
    /// 1. the declared ABI revision matches this build's;
    /// 2. the runtime struct size matches the pinned [`JIT_HELPERS_ABI_SIZE`];
    /// 3. every [`HelperFieldDesc::required`] slot is non-zero;
    /// 4. every [`HelperKind::Offset`] slot is a plausible displacement
    ///    (`<= `[`MAX_PLAUSIBLE_OFFSET`]).
    ///
    /// Returns the *first* problem found; iteration order is ABI order, so the
    /// name reported is the earliest offending slot. Use
    /// [`JitRuntimeHelpers::null_pointers`] when you want the complete list of
    /// missing required helpers instead.
    ///
    /// [`HelperKind::Constant`] slots are not range-checked: a baked address is
    /// legitimately anywhere in the address space, and zero legitimately means
    /// "mechanism disabled".
    pub fn validate_with(&self, abi_version: u32) -> Result<(), HelperAbiError> {
        if abi_version != JIT_HELPERS_ABI_VERSION {
            return Err(HelperAbiError::VersionMismatch {
                expected: JIT_HELPERS_ABI_VERSION,
                found: abi_version,
            });
        }
        let size = core::mem::size_of::<JitRuntimeHelpers>();
        if size != JIT_HELPERS_ABI_SIZE {
            return Err(HelperAbiError::SizeMismatch {
                expected: JIT_HELPERS_ABI_SIZE,
                found: size,
            });
        }
        for desc in HELPER_FIELDS {
            let value = self.word_at(desc);
            if desc.required && value == 0 {
                return Err(HelperAbiError::MissingRequired(desc.name));
            }
            if desc.kind == HelperKind::Offset && value > MAX_PLAUSIBLE_OFFSET {
                return Err(HelperAbiError::ImplausibleOffset {
                    name: desc.name,
                    value,
                });
            }
        }
        Ok(())
    }

    /// [`JitRuntimeHelpers::validate_with`] using this build's
    /// [`JIT_HELPERS_ABI_VERSION`].
    ///
    /// Correct only when producer and consumer are the same binary, which is
    /// the in-process case. A separately-compiled consumer must call
    /// `validate_with` with the version *it* was built against.
    pub fn validate_abi(&self) -> Result<(), HelperAbiError> {
        self.validate_with(JIT_HELPERS_ABI_VERSION)
    }
}

/// Look up a slot descriptor by field name.
///
/// Linear over 58 entries; intended for diagnostics and tooling, not for a hot
/// path.
pub fn helper_field(name: &str) -> Option<&'static HelperFieldDesc> {
    HELPER_FIELDS.iter().find(|d| d.name == name)
}

/// Byte offset of a slot by field name — the number the JIT bakes as `disp32`.
pub fn helper_field_offset(name: &str) -> Option<usize> {
    helper_field(name).map(|d| d.offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FieldKind;
    use core::mem::offset_of;

    type H = JitRuntimeHelpers;

    /// Every descriptor offset, compared against an independently written
    /// `offset_of!` probe for the same field. The table itself is built with
    /// `offset_of!`, so this additionally pins the *order* of the rows: probe
    /// `i` is written out by hand in ABI order, and a reordered table row
    /// would land on the wrong probe.
    #[test]
    fn helper_fields_offsets_match_offset_of() {
        let probes: [(&str, usize); H::NUM_FIELDS] = [
            ("newarray", offset_of!(H, newarray)),
            ("new_object", offset_of!(H, new_object)),
            ("anewarray_object", offset_of!(H, anewarray_object)),
            ("baload", offset_of!(H, baload)),
            ("bastore", offset_of!(H, bastore)),
            ("iaload", offset_of!(H, iaload)),
            ("iastore", offset_of!(H, iastore)),
            ("aaload", offset_of!(H, aaload)),
            ("aastore", offset_of!(H, aastore)),
            ("multianewarray_2d", offset_of!(H, multianewarray_2d)),
            ("arraylength", offset_of!(H, arraylength)),
            ("getfield", offset_of!(H, getfield)),
            ("putfield_int", offset_of!(H, putfield_int)),
            ("putfield_long", offset_of!(H, putfield_long)),
            ("putfield_float", offset_of!(H, putfield_float)),
            ("putfield_double", offset_of!(H, putfield_double)),
            ("putfield_object", offset_of!(H, putfield_object)),
            ("getstatic", offset_of!(H, getstatic)),
            ("putstatic_int", offset_of!(H, putstatic_int)),
            ("putstatic_long", offset_of!(H, putstatic_long)),
            ("putstatic_float", offset_of!(H, putstatic_float)),
            ("putstatic_double", offset_of!(H, putstatic_double)),
            ("putstatic_object", offset_of!(H, putstatic_object)),
            ("checkcast", offset_of!(H, checkcast)),
            ("instanceof_check", offset_of!(H, instanceof_check)),
            ("throw_aioobe", offset_of!(H, throw_aioobe)),
            ("throw_arithmetic", offset_of!(H, throw_arithmetic)),
            ("invoke_dispatch", offset_of!(H, invoke_dispatch)),
            ("invoke_virtual_mic", offset_of!(H, invoke_virtual_mic)),
            ("lambda_int_to_double", offset_of!(H, lambda_int_to_double)),
            ("write_barrier", offset_of!(H, write_barrier)),
            ("satb_pre_write_barrier", offset_of!(H, satb_pre_write_barrier)),
            ("uncommon_trap", offset_of!(H, uncommon_trap)),
            ("math_fma_double", offset_of!(H, math_fma_double)),
            ("math_fma_float", offset_of!(H, math_fma_float)),
            (
                "tlab_cursor_offset_in_thread",
                offset_of!(H, tlab_cursor_offset_in_thread),
            ),
            (
                "tlab_end_offset_in_thread",
                offset_of!(H, tlab_end_offset_in_thread),
            ),
            ("class_id_offset_in_obj", offset_of!(H, class_id_offset_in_obj)),
            ("get_current_thread", offset_of!(H, get_current_thread)),
            ("tlab_post_init", offset_of!(H, tlab_post_init)),
            ("frame_record", offset_of!(H, frame_record)),
            (
                "shadow_stack_offset_in_thread",
                offset_of!(H, shadow_stack_offset_in_thread),
            ),
            ("throw_exception", offset_of!(H, throw_exception)),
            ("jit_npe_with_action", offset_of!(H, jit_npe_with_action)),
            ("dispatch_threw", offset_of!(H, dispatch_threw)),
            ("jit_frem", offset_of!(H, jit_frem)),
            ("jit_drem", offset_of!(H, jit_drem)),
            ("self_call_stack_guard", offset_of!(H, self_call_stack_guard)),
            ("region_bounds_addr", offset_of!(H, region_bounds_addr)),
            ("native_stack_floor_fn", offset_of!(H, native_stack_floor_fn)),
            ("ldc_string", offset_of!(H, ldc_string)),
            ("safepoint_flag_addr", offset_of!(H, safepoint_flag_addr)),
            ("safepoint_slow_path", offset_of!(H, safepoint_slow_path)),
            ("jit_card_table_addr", offset_of!(H, jit_card_table_addr)),
            ("jit_card_old_base", offset_of!(H, jit_card_old_base)),
            ("jit_card_old_end", offset_of!(H, jit_card_old_end)),
            ("set_throw_bci", offset_of!(H, set_throw_bci)),
            ("service_callee_deopt", offset_of!(H, service_callee_deopt)),
        ];

        assert_eq!(HELPER_FIELDS.len(), probes.len());
        for (i, (name, offset)) in probes.iter().enumerate() {
            let desc = &HELPER_FIELDS[i];
            assert_eq!(
                desc.name, *name,
                "HELPER_FIELDS row {} is `{}`, expected `{}` — the ABI order \
                 changed",
                i, desc.name, name,
            );
            assert_eq!(
                desc.offset, *offset,
                "HELPER_FIELDS offset for `{}` is {}, offset_of! says {}",
                name, desc.offset, offset,
            );
            assert_eq!(
                desc.offset,
                i * HELPER_FIELD_STRIDE,
                "field `{}` is not at its sequential JIT ABI offset",
                name,
            );
        }
    }

    /// The literal size and alignment the JIT's baked offsets assume. Written
    /// as bare numbers on purpose: an accidental layout change must fail here
    /// loudly rather than be absorbed by a computed expression.
    #[test]
    fn helper_table_size_and_align_are_the_literal_abi_numbers() {
        assert_eq!(core::mem::size_of::<H>(), 464);
        assert_eq!(core::mem::align_of::<H>(), 8);
        assert_eq!(JIT_HELPERS_ABI_SIZE, 464);
        assert_eq!(JIT_HELPERS_ABI_ALIGN, 8);
        assert_eq!(HELPER_FIELD_STRIDE, 8);
        assert_eq!(NUM_HELPER_FIELDS, 58);
        assert_eq!(H::NUM_FIELDS, 58);
        assert_eq!(H::NUM_HELPER_FN_FIELDS, 49);
        assert_eq!(JIT_HELPERS_ABI_VERSION, 1);
    }

    /// The descriptor table and the crate root's `helper_fields!` list are two
    /// independent transcriptions of the same struct. Neither may drift.
    #[test]
    fn helper_fields_agree_with_crate_root() {
        let h = H::default();
        let root = h.all_fields();
        assert_eq!(root.len(), HELPER_FIELDS.len());
        for (i, entry) in root.iter().enumerate() {
            let desc = &HELPER_FIELDS[i];
            assert_eq!(desc.name, entry.name, "field name drift at index {}", i);
            match entry.kind {
                FieldKind::RequiredPtr => {
                    assert_eq!(desc.kind, HelperKind::Function, "{}", desc.name);
                    assert!(desc.required, "{} should be required", desc.name);
                }
                FieldKind::OptionalPtr => {
                    assert_eq!(desc.kind, HelperKind::Function, "{}", desc.name);
                    assert!(!desc.required, "{} should be optional", desc.name);
                }
                FieldKind::Offset => {
                    assert!(
                        desc.kind == HelperKind::Offset || desc.kind == HelperKind::Constant,
                        "{} should be a non-callable slot",
                        desc.name,
                    );
                    assert!(!desc.required, "{} should not be required", desc.name);
                }
            }
        }
    }

    #[test]
    fn validate_with_rejects_a_zeroed_table_naming_the_first_gap() {
        let h = H::default();
        // `newarray` is slot 0 and required, so it is the first miss.
        assert_eq!(
            h.validate_with(JIT_HELPERS_ABI_VERSION),
            Err(HelperAbiError::MissingRequired("newarray")),
        );
        assert_eq!(h.validate_abi(), Err(HelperAbiError::MissingRequired("newarray")));
        // The message must name the field.
        let msg = HelperAbiError::MissingRequired("newarray").to_string();
        assert!(msg.contains("newarray"), "unhelpful message: {}", msg);
    }

    #[test]
    fn validate_with_reports_the_next_gap_once_the_first_is_wired() {
        let mut h = H::default();
        h.newarray = 0x1000;
        assert_eq!(
            h.validate_with(JIT_HELPERS_ABI_VERSION),
            Err(HelperAbiError::MissingRequired("new_object")),
        );
    }

    #[test]
    fn validate_with_rejects_a_foreign_abi_version() {
        // The version check runs before the field scan, so even a fully wired
        // table is rejected.
        let h = fully_wired();
        assert_eq!(
            h.validate_with(JIT_HELPERS_ABI_VERSION + 1),
            Err(HelperAbiError::VersionMismatch {
                expected: JIT_HELPERS_ABI_VERSION,
                found: JIT_HELPERS_ABI_VERSION + 1,
            }),
        );
    }

    #[test]
    fn validate_with_accepts_a_wired_table_with_zero_optionals() {
        // Every optional slot and every offset stays 0: that is the documented
        // "not wired" state and must not be an error.
        let h = fully_wired();
        assert_eq!(h.validate_with(JIT_HELPERS_ABI_VERSION), Ok(()));
    }

    #[test]
    fn validate_with_rejects_a_pointer_stored_in_an_offset_slot() {
        let mut h = fully_wired();
        h.tlab_cursor_offset_in_thread = 0x7fff_1234_5678;
        assert_eq!(
            h.validate_with(JIT_HELPERS_ABI_VERSION),
            Err(HelperAbiError::ImplausibleOffset {
                name: "tlab_cursor_offset_in_thread",
                value: 0x7fff_1234_5678,
            }),
        );
    }

    #[test]
    fn accessors_return_none_for_zeroed_slots() {
        let h = H::default();
        assert!(h.newarray_fn().is_none());
        assert!(h.invoke_virtual_mic_fn().is_none());
        assert!(h.math_fma_double_fn().is_none());
        assert!(h.get_current_thread_fn().is_none());
        assert!(h.frame_record_fn().is_none());
        assert!(h.ldc_string_fn().is_none());
        assert!(h.native_stack_floor_fn_fn().is_none());
        assert!(h.service_callee_deopt_fn().is_none());
        assert!(h.safepoint_slow_path_fn().is_none());
    }

    /// A wired slot round-trips: the accessor hands back a pointer whose
    /// address is the one that was stored, with no offset confusion.
    #[test]
    fn accessors_round_trip_a_wired_slot() {
        unsafe extern "C" fn stub_arraylength(_array_ptr: i64) -> i64 {
            7
        }
        unsafe extern "C" fn stub_drem(a: f64, b: f64) -> f64 {
            a % b
        }

        let mut h = H::default();
        h.arraylength = stub_arraylength as *const () as usize;
        h.jit_drem = stub_drem as *const () as usize;

        let f = h.arraylength_fn().expect("wired slot must be Some");
        assert_eq!(f as usize, h.arraylength);
        // SAFETY: the slot holds `stub_arraylength`, which matches
        // `HelperFnArraylength` exactly and dereferences none of its arguments.
        assert_eq!(unsafe { f(0) }, 7);

        let g = h.jit_drem_fn().expect("wired slot must be Some");
        assert_eq!(g as usize, h.jit_drem);
        // SAFETY: as above — `stub_drem` matches `HelperFnJitDrem` and is pure.
        assert_eq!(unsafe { g(5.0, 3.0) }, 2.0);

        // Neighbouring slots stay unwired: the accessors read their own field.
        assert!(h.aastore_fn().is_none());
        assert!(h.jit_frem_fn().is_none());
    }

    #[test]
    fn word_at_reads_the_slot_the_descriptor_names() {
        let mut h = H::default();
        h.set_throw_bci = 0xdead_beef;
        let desc = helper_field("set_throw_bci").expect("described slot");
        assert_eq!(h.word_at(desc), 0xdead_beef);
        assert_eq!(h.as_words()[desc.offset / HELPER_FIELD_STRIDE], 0xdead_beef);
        assert_eq!(helper_field_offset("set_throw_bci"), Some(desc.offset));
        assert!(helper_field("no_such_helper").is_none());
    }

    #[test]
    fn as_words_matches_the_struct_fields() {
        let mut h = H::default();
        h.newarray = 1;
        h.service_callee_deopt = 2;
        let w = h.as_words();
        assert_eq!(w[0], 1, "first slot");
        assert_eq!(w[H::NUM_FIELDS - 1], 2, "last slot");
        assert_eq!(w.len(), 58);
    }

    /// Build a table with every *required* slot non-zero and every optional
    /// slot left at the documented `0`.
    fn fully_wired() -> H {
        let mut h = H::default();
        for (i, desc) in HELPER_FIELDS.iter().enumerate() {
            if desc.required {
                // SAFETY: `i < NUM_HELPER_FIELDS` and the struct's layout is
                // exactly `[usize; NUM_HELPER_FIELDS]` (see `as_words`), so the
                // write is in bounds, correctly typed and correctly aligned.
                // The pointer is re-derived from a fresh `&mut h` on every
                // iteration, so no stale provenance is carried across writes.
                // The value is a dummy non-zero address that is never called.
                unsafe { *(&mut h as *mut H as *mut usize).add(i) = 0x1000 + i };
            }
        }
        h
    }
}
