// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Typed ABI description for [`JitRuntimeHelpers`].
//!
//! [`JitRuntimeHelpers`] is, and must remain, a `#[repr(C)]` table of bare
//! `usize` words. What is actually baked into generated RWX machine code, and
//! what therefore is and is not load-bearing, is worth stating precisely —
//! the previous version of this paragraph overstated it, which made the
//! weakest link look like the strongest.
//!
//! **What the emitter bakes.** The `jit` crate holds the table *by value*
//! (`helpers: JitRuntimeHelpers` in `jit/src/x64.rs`) and reads each slot by
//! Rust field name. `emit_call_absolute` then bakes the helper's **absolute
//! address** as a `rel32` displacement (or a 12-byte `imm64` form out of
//! ±2 GiB reach). There is no `CALL [helpers + disp32]` anywhere in the
//! backend. So within this workspace, where producer and consumer are the same
//! Rust type compiled from this file, a field *reorder* alone does not
//! mis-target a call — the compiler follows the reorder on both sides.
//!
//! **What is load-bearing anyway.**
//!
//! * The **signature** of each slot. The backend loads N argument registers by
//!   hand at each call site and jumps to a bare address; nothing in the type
//!   system connects that hand-written setup to the callee's real arity,
//!   argument widths, or whether it returns a value at all. This is the live
//!   hazard, and it is why [`HELPER_FN_SIGS`] exists.
//! * The **byte offsets**, for `as_words`/`word_at` (which reinterpret the
//!   struct as `[usize; NUM_FIELDS]` and index it) and for any future
//!   out-of-process or non-Rust producer. `#[repr(C)]` plus the all-`usize`
//!   rule is what makes that reinterpretation sound.
//! * The **append-only rule**, because a removal or an insert in the middle
//!   rebases every later offset at once, and because the diff of such a change
//!   looks small.
//!
//! Changing a field's Rust *type* would change nothing about the emitted code
//! but would break the `size_of::<JitRuntimeHelpers>() / 8` identity every
//! offset assertion in this crate depends on.
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
//! * [`HELPER_FN_SIGS`], the per-slot arity / argument-class / returns-a-value
//!   shape, derived from those same aliases, so the facts a call site needs are
//!   available as data rather than only as a type;
//! * [`GOLDEN_HELPER_OFFSETS`], the frozen name → byte-offset contract written
//!   as literals — the one check here that a *coordinated* reorder of the
//!   struct and all its derived tables cannot satisfy;
//! * [`ABI_REVISIONS`], the shape ledger that makes appending a field without
//!   bumping [`JIT_HELPERS_ABI_VERSION`] a compile error;
//! * [`JitRuntimeHelpers::validate_with`], a version-checked validator that
//!   names the first missing required slot.
//!
//! **The validator is not armed.** Nothing outside this crate calls
//! [`JitRuntimeHelpers::validate_abi`], [`JitRuntimeHelpers::validate`] or
//! [`JitRuntimeHelpers::null_pointers`] — the runtime half of this contract
//! exists and is tested, but never runs in a real VM. See
//! `docs/jit/helper-abi.md`.
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
/// `6` is the revision of the 65-field, 520-byte table shipped today. The
/// full history is in [`ABI_REVISIONS`], which a const assertion ties to this
/// constant, to [`NUM_HELPER_FIELDS`] and to [`JIT_HELPERS_ABI_SIZE`] — so
/// appending a field without bumping this number no longer compiles.
///
/// (Revision `2` shipped the 60-field table; the `monitor_enter`/`monitor_exit`
/// append that made it 62 did not bump this constant, because at the time
/// nothing checked it. `ABI_REVISIONS` is that check.)
pub const JIT_HELPERS_ABI_VERSION: u32 = 12;

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
// Signature vocabulary.
//
// The `HelperFn*` aliases carry each slot's real C signature, but a type
// alias is not something a const assertion — or the emitter — can ask
// questions of. These two traits turn "what does this argument cost in the
// calling convention" into a value the macro can fold into `HELPER_FN_SIGS`.
//
// They are deliberately NOT blanket-implemented. A helper whose signature
// uses a type not listed here fails to compile with "the trait bound is not
// satisfied" — which is the correct outcome: a new argument or return type is
// a calling-convention decision that must be made explicitly, not absorbed.
// ---------------------------------------------------------------------

/// Calling-convention class of one helper argument.
///
/// Implemented only for the types the helper ABI actually uses. Adding a
/// helper whose argument is of any other type is a compile error until the
/// type is classified here — see the module-level note on fail-closed
/// classification.
pub trait HelperArgAbi {
    /// `true` when the argument travels in an XMM register rather than an
    /// integer ARG_REG. The two register files are assigned independently on
    /// SysV but *positionally* on Win64, which is why
    /// [`HelperFnSig::float_args`] is pinned rather than merely recorded.
    const IS_FLOAT: bool;
    /// Width in bytes of the argument as the callee reads it.
    const WIDTH: usize;
}

/// Calling-convention class of a helper's return value.
///
/// Implemented only for the return types the helper ABI actually uses; see
/// [`HelperArgAbi`] for why there is no blanket impl.
pub trait HelperRetAbi {
    /// `false` only for `()`. A call site that reserves a result register for
    /// a `()`-returning helper reads whatever the callee left in RAX.
    const RETURNS_VALUE: bool;
    /// `true` when the result comes back in XMM0 rather than RAX.
    const IS_FLOAT: bool;
}

macro_rules! impl_helper_arg_abi {
    ( $( $t:ty => $is_float:expr ),* $(,)? ) => {
        $(
            impl HelperArgAbi for $t {
                const IS_FLOAT: bool = $is_float;
                const WIDTH: usize = core::mem::size_of::<$t>();
            }
        )*
    };
}

impl_helper_arg_abi! {
    i64 => false,
    usize => false,
    f32 => true,
    f64 => true,
    *const u8 => false,
}

impl HelperRetAbi for () {
    const RETURNS_VALUE: bool = false;
    const IS_FLOAT: bool = false;
}
impl HelperRetAbi for i64 {
    const RETURNS_VALUE: bool = true;
    const IS_FLOAT: bool = false;
}
impl HelperRetAbi for f32 {
    const RETURNS_VALUE: bool = true;
    const IS_FLOAT: bool = true;
}
impl HelperRetAbi for f64 {
    const RETURNS_VALUE: bool = true;
    const IS_FLOAT: bool = true;
}
impl HelperRetAbi for *mut c_void {
    const RETURNS_VALUE: bool = true;
    const IS_FLOAT: bool = false;
}

/// Signature shape of one callable slot, derived from `helper_fn_slots!`.
///
/// Everything here is *derived from the `HelperFn*` alias*, not transcribed:
/// there is no second list to keep in step. What the emitter does with it is
/// still on the emitter — see `docs/jit/helper-abi.md` for the check
/// that belongs in `jit/` and cannot live in this crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelperFnSig {
    /// Rust field name on [`JitRuntimeHelpers`].
    pub field: &'static str,
    /// Name of the `<field>_fn` accessor.
    pub accessor: &'static str,
    /// Name of the `HelperFn*` type alias carrying the full signature.
    pub alias: &'static str,
    /// Number of declared parameters. This is the number of argument
    /// registers the call site must load — no helper takes a by-value
    /// aggregate, which is pinned by [`HelperArgAbi`] having no impl for one.
    pub arity: usize,
    /// How many of those parameters are floating-point.
    pub float_args: usize,
    /// `false` only for a `-> ()` helper.
    pub returns_value: bool,
    /// `true` when the result is in XMM0.
    pub returns_float: bool,
}

impl HelperFnSig {
    /// Integer/pointer parameters — the ones that consume an integer ARG_REG.
    pub const fn int_args(&self) -> usize {
        self.arity - self.float_args
    }

    /// Parameters that do NOT fit the Win64 four-register integer file and so
    /// must be written to the stack by the call site (`[RSP+32]`, `[RSP+40]`,
    /// …) in addition to the 32-byte shadow space.
    ///
    /// Zero for every helper but `invoke_virtual_mic`; see
    /// [`HELPERS_NEEDING_WIN64_STACK_ARGS`].
    pub const fn win64_stack_args(&self) -> usize {
        if self.arity > WIN64_INT_ARG_REGS {
            self.arity - WIN64_INT_ARG_REGS
        } else {
            0
        }
    }
}

/// Integer argument registers available on Win64 (RCX, RDX, R8, R9).
///
/// Beyond this the caller must place arguments on the stack above the 32-byte
/// shadow space. Mirrors `CALL_ARG_REGS` in `jit/src/ir_lower.rs`.
pub const WIN64_INT_ARG_REGS: usize = 4;

/// Integer argument registers available on SysV x86-64
/// (RDI, RSI, RDX, RCX, R8, R9). The upper bound on helper arity.
pub const SYSV_INT_ARG_REGS: usize = 6;

/// Byte-wise `&str` equality usable in a `const` context.
///
/// `==` on `&str` is not const-callable, and every cross-table name check in
/// this module has to run at compile time to be worth anything.
pub const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// `accessor == concat!(field, "_fn")`, in a `const` context.
///
/// The naming rule is mechanical on purpose: it is the only thing that ties a
/// `helper_fn_slots!` row's getter to its field without a second list.
pub const fn accessor_name_matches_field(accessor: &str, field: &str) -> bool {
    let (a, f) = (accessor.as_bytes(), field.as_bytes());
    if a.len() != f.len() + 3 {
        return false;
    }
    let mut i = 0;
    while i < f.len() {
        if a[i] != f[i] {
            return false;
        }
        i += 1;
    }
    a[f.len()] == b'_' && a[f.len() + 1] == b'f' && a[f.len() + 2] == b'n'
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

            /// Every `<field>_fn` accessor, invoked, paired with the name of
            /// the field it is *supposed* to read.
            ///
            /// Generated from the same macro rows as the accessors themselves,
            /// so it cannot omit one. The point is not the values but the
            /// pairing: `accessor_reads_only_its_own_slot` wires exactly one
            /// callable slot at a time and asserts that exactly one probe entry
            /// goes `Some`, and that it is the entry named for that slot. That
            /// is what catches two swapped rows in `helper_fn_slots!` — a swap
            /// that gives one helper another helper's signature while every
            /// count, offset and census check still passes.
            #[cfg(test)]
            fn helper_fn_accessor_probe(
                &self,
            ) -> [(&'static str, Option<usize>); Self::NUM_HELPER_FN_FIELDS] {
                [
                    $( (stringify!($field), self.$getter().map(|f| f as usize)), )*
                ]
            }
        }

        /// Per-slot C signature shape for every callable slot, derived from the
        /// same macro rows that declare the `HelperFn*` aliases.
        ///
        /// This is the information the emitter needs and currently re-derives
        /// by hand at each call site: how many argument registers to load,
        /// whether they are integer or floating, and whether anything comes
        /// back. See [`HelperFnSig`] for what each field pins.
        pub const HELPER_FN_SIGS: &[HelperFnSig] = &[
            $(
                HelperFnSig {
                    field: stringify!($field),
                    accessor: stringify!($getter),
                    alias: stringify!($alias),
                    arity: 0 $( + helper_fn_slots!(@one $arg) )*,
                    float_args: 0 $( + (<$arg as HelperArgAbi>::IS_FLOAT as usize) )*,
                    returns_value: <$ret as HelperRetAbi>::RETURNS_VALUE,
                    returns_float: <$ret as HelperRetAbi>::IS_FLOAT,
                },
            )*
        ];

        // One pin per row, naming the row in its own failure message.
        //
        // (a) The accessor name must be the field name plus `_fn`. The row
        //     supplies both independently, so a row edited to point a getter at
        //     a different field — the swap that hands a helper another helper's
        //     signature — stops compiling here instead of miscompiling later.
        //     TRIPS ON: `HelperFnMonitorEnter, monitor_exit, monitor_enter_fn;`.
        //
        // (b) Every integer or pointer argument is a full machine word. A
        //     narrower one is a silent ABI lie: the emitter loads the whole
        //     64-bit ARG_REG and the callee would read only part of it. Floats
        //     are exempt — `f32` is a legitimately 4-byte scalar in XMM.
        //     TRIPS ON: declaring an argument `i32`, `u32`, `bool`, ….
        $(
            const _: () = {
                assert!(
                    accessor_name_matches_field(stringify!($getter), stringify!($field)),
                    concat!(
                        "the `", stringify!($getter), "` accessor is not named \
                         after the `", stringify!($field), "` field it reads — \
                         this helper_fn_slots! row pairs a getter with the \
                         wrong field, which would give that helper another \
                         helper's C signature",
                    ),
                );
                let args_are_machine_words = true
                    $( && (<$arg as HelperArgAbi>::IS_FLOAT
                           || <$arg as HelperArgAbi>::WIDTH == 8) )*;
                assert!(
                    args_are_machine_words,
                    concat!(
                        "an argument of the `", stringify!($field), "` helper \
                         is narrower than a machine word — the JIT loads the \
                         full 64-bit ARG_REG, so a narrow integer parameter \
                         here is an ABI lie even though the emitted code is \
                         unchanged",
                    ),
                );
            };
        )*
    };
    (@unit $field:ident) => { () };
    (@one $arg:ty) => { 1usize };
}

helper_fn_slots! {
    // Allocation. (vm_ptr, atype, length) / (vm_ptr, class_id, num_fields) /
    // (vm_ptr, component_class_id, length).
    HelperFnNewarray, newarray, newarray_fn, (i64, i64, i64) -> i64;
    HelperFnNewObject, new_object, new_object_fn, (i64, i64, i64) -> i64;
    HelperFnAnewarrayObject, anewarray_object, anewarray_object_fn, (i64, i64, i64) -> i64;

    // Array access. Primitive loads: (array_ptr, index). Primitive stores:
    // (array_ptr, index, val). `aaload` and `aastore` additionally take
    // `vm_ptr` FIRST, for the symmetric reason: a reference LOAD runs the ZGC
    // read barrier and a reference STORE runs the write barrier, and both need
    // a `&VmHeap` to reach one.
    //
    // `aaload` gained its `vm_ptr` on 2026-09-01 (`.agent-requests/B8-abi.txt`).
    // A JIT-helper ABI change is normally expensive; this one was affordable
    // because NO emitter calls `helpers.aaload`. `aaload` is lowered inline by
    // `jit/src/x64/arrays.rs::emit_ref_aload_regs`, `grep -rn "helpers\.aaload"
    // jit/src` is empty, and the aarch64 backend does not call it either -- so
    // there was no emitted `call` whose argument registers had to move and no
    // `stack_arg_block_size` / shadow-space accounting to revisit. Nothing
    // consumes the declared arity except this file's own assertions (3 <= 4, so
    // `HELPERS_NEEDING_WIN64_STACK_ARGS` is unchanged).
    //
    // The row still has to be honest BEFORE anything routes `aaload` back to
    // the helper under an armed barrier, which is what
    // `let _: HelperFnAaload = jit_aaload;` in `vm/src/jit/helpers.rs`
    // enforces: the two halves cannot disagree and still compile.
    HelperFnBaload, baload, baload_fn, (i64, i64) -> i64;
    HelperFnBastore, bastore, bastore_fn, (i64, i64, i64) -> ();
    HelperFnIaload, iaload, iaload_fn, (i64, i64) -> i64;
    HelperFnIastore, iastore, iastore_fn, (i64, i64, i64) -> ();
    HelperFnAaload, aaload, aaload_fn, (i64, i64, i64) -> i64;
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

    // Constant-pool-indexed allocation, for a `new`/`anewarray` whose target
    // class was not loaded when the method compiled — (vm_ptr,
    // holder_class_id, cp_idx), plus the array length for the `anewarray`
    // form. See `JitRuntimeHelpers::new_object_cp`.
    HelperFnNewObjectCp, new_object_cp, new_object_cp_fn, (i64, i64, i64) -> i64;
    HelperFnAnewarrayObjectCp, anewarray_object_cp, anewarray_object_cp_fn,
        (i64, i64, i64, i64) -> i64;
    // (vm_ptr, obj) -> possibly-remapped obj. The return is not advisory:
    // a contended acquire can move the object while the thread is parked.
    HelperFnMonitorEnter, monitor_enter, monitor_enter_fn, (i64, i64) -> i64;
    HelperFnMonitorExit, monitor_exit, monitor_exit_fn, (i64, i64) -> i64;
    // Constant-pool-indexed `ldc <Class>` — (vm_ptr, holder_class_id, cp_idx)
    // -> mirror ObjectRef, `0` after publishing a pending exception. Same
    // shape as `new_object_cp` and for the same reason; see
    // `JitRuntimeHelpers::ldc_class_cp`.
    HelperFnLdcClassCp, ldc_class_cp, ldc_class_cp_fn, (i64, i64, i64) -> i64;
    // `(vm_ptr, holder_class_id, cp_idx) -> ObjectRef | 0`, the string twin of
    // the row above. Same three-register shape, same `0 = pending exception`
    // convention. See `JitRuntimeHelpers::ldc_string_cp`.
    HelperFnLdcStringCp, ldc_string_cp, ldc_string_cp_fn, (i64, i64, i64) -> i64;
    // `(seg, index, kind, out_ptr) -> 1 handled | 0 declined` — the FFM
    // element READ fast path. A decline leaves the site's native dispatch to
    // run unchanged, so every case it does not recognise keeps today's
    // behaviour. See `JitRuntimeHelpers::ffm_segment_get`.
    HelperFnFfmSegmentGet, ffm_segment_get, ffm_segment_get_fn, (i64, i64, i64, i64) -> i64;
    // `(seg, index, kind, raw_value) -> 1 handled | 0 declined` — the WRITE
    // twin. See `JitRuntimeHelpers::ffm_segment_set`.
    HelperFnFfmSegmentSet, ffm_segment_set, ffm_segment_set_fn, (i64, i64, i64, i64) -> i64;
    // `(vm_ptr, obj_ptr, val_ptr)` — G1's post-write barrier, called from the
    // inline barrier's slow arm. See `JitRuntimeHelpers::g1_post_write_barrier`
    // for why it is not `write_barrier`.
    HelperFnG1PostWriteBarrier, g1_post_write_barrier, g1_post_write_barrier_fn, (i64, i64, i64) -> ();
    // JVMS §6.5 aastore covariance check ONLY — (vm_ptr, array_ptr, val) ->
    // `i64::MIN` = refused (ArrayStoreException published) / `0` = proceed.
    // NOT the store: the caller keeps the inline MOV, the SATB pre-write
    // barrier and the card mark. See `JitRuntimeHelpers::aastore_type_check`.
    HelperFnAastoreTypeCheck, aastore_type_check, aastore_type_check_fn, (i64, i64, i64) -> i64;
    // Compiled local exception handlers -- (vm_ptr, site_ptr, out_exc_slot) ->
    // the index of the matching candidate in this throwing bci's own handler
    // list, or `-1` to propagate. The third argument is a WRITABLE frame
    // address, not a value: on a hit the helper stores the throwable there,
    // which is the operand slot the handler block starts from. See
    // `JitRuntimeHelpers::local_handler_lookup`.
    HelperFnLocalHandlerLookup, local_handler_lookup, local_handler_lookup_fn, (i64, i64, i64) -> i64;
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
    // Constant-pool-indexed allocation. Optional: a hand-built test table
    // leaves these zero, and the backend then refuses a deferred
    // (not-yet-loaded) `new`/`anewarray` site instead of emitting a CALL to 0.
    (new_object_cp,                  Function, false),
    (anewarray_object_cp,            Function, false),
    // Optional: 0 makes `ir_lower` refuse monitor ops rather than drop them.
    (monitor_enter,                  Function, false),
    (monitor_exit,                   Function, false),
    // Optional: 0 makes the single-pass backend refuse an `ldc <Class>` site.
    (ldc_class_cp,                   Function, false),
    // Required: the x64 `0x53` lowering is inline and calls this for the JVMS
    // §6.5 covariance check, so a `0` slot would be a reference store with no
    // check at all — the heap-type-confusion defect the slot exists to close.
    (aastore_type_check,             Function, true),
    // Baked absolute address of the GC's JIT_READ_BOUNDS table, not callable.
    // A DIFFERENT table from region_bounds_addr above: that one gates inline
    // reference STORES and G1/ZGC keep it empty on purpose (G1-2); this one
    // answers the READ question -- is this address mapped -- and G1 does fill it.
    (read_bounds_addr,               Constant, false),
    // Optional: 0 makes the single-pass backend arm no local-handler stubs, so
    // every caught exception keeps leaving compiled code through the reason-9
    // deopt / shared-sentinel route -- the pre-feature behaviour.
    (local_handler_lookup,           Function, false),
    // `(vm_ptr, holder_class_id, cp_idx) -> ObjectRef | 0`. Optional: a 0 makes
    // both backends refuse a string-`ldc` site, exactly as an unwired
    // `ldc_class_cp` makes them refuse a class-`ldc` one.
    (ldc_string_cp,                  Function, false),
    (ffm_segment_get,                Function, false),
    (ffm_segment_set,                Function, false),
    // Reference-store barrier gates. NOT functions: each is the address of a
    // collector-owned gate BYTE that compiled code reads to decide whether a
    // barrier CALL can be skipped. Optional in the strongest sense -- 0 means
    // "this collector published no plan" and every emitter arm keeps its
    // full-helper path.
    (ref_store_pre_gate,             Constant, false),
    (ref_store_post_gate,            Constant, false),
    (ref_store_post_young_floor,     Constant, false),
    // Address of the GC's JIT_G1_BARRIER table, not a call target.
    (g1_barrier_addr,                Constant, false),
    (g1_post_write_barrier,          Function, false),
    // Not a pointer: a 0/1 flag saying the collector keeps an object-start
    // registry, so an inline-TLAB allocation must call `tlab_post_init` (which
    // registers it) instead of taking the skip-the-helper fast path.
    (tlab_registration_required,     Constant, false),
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
    NUM_HELPER_FIELDS == 75,
    "JitRuntimeHelpers field count changed — bump JIT_HELPERS_ABI_VERSION, the \
     literal here, and the size literal below",
);

// Pin the literal size and alignment. The JIT bakes `disp32` offsets derived
// from this layout into RWX memory; a silent change here is a wild call.
const _: () = assert!(
    JIT_HELPERS_ABI_SIZE == 600,
    "JitRuntimeHelpers size changed (expected 75 * 8 = 600) — the JIT's baked \
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
        required == 43,
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

// ---------------------------------------------------------------------
// The golden name → offset table.
//
// Everything above derives offsets from the struct: `offset_of!` follows a
// reorder, and `index * 8` follows a reorder that also reorders the descriptor
// rows. A contributor who moves a field and dutifully moves its row in
// `helper_field_table!`, in `helper_fn_slots!` and in the probe list therefore
// passes every check in this crate while changing the binary contract.
//
// These are LITERAL numbers keyed by NAME. They are the one place a human must
// change a value that no expression can recompute. Do not "fix" a failure here
// by editing the number: a failure means the field genuinely moved, and moving
// a field is the thing this table exists to prevent.
//
// Appending a helper adds ONE row at the end with offset `previous + 8`.
// ---------------------------------------------------------------------

/// The frozen byte offset of every [`JitRuntimeHelpers`] slot, by name.
///
/// Keyed by name and written as literals, so unlike every other check in this
/// module it survives a coordinated reorder of the struct and all its
/// descriptor tables — which is the one reorder every other check here misses.
pub const GOLDEN_HELPER_OFFSETS: [(&str, usize); NUM_HELPER_FIELDS] = [
    ("newarray", 0),
    ("new_object", 8),
    ("anewarray_object", 16),
    ("baload", 24),
    ("bastore", 32),
    ("iaload", 40),
    ("iastore", 48),
    ("aaload", 56),
    ("aastore", 64),
    ("multianewarray_2d", 72),
    ("arraylength", 80),
    ("getfield", 88),
    ("putfield_int", 96),
    ("putfield_long", 104),
    ("putfield_float", 112),
    ("putfield_double", 120),
    ("putfield_object", 128),
    ("getstatic", 136),
    ("putstatic_int", 144),
    ("putstatic_long", 152),
    ("putstatic_float", 160),
    ("putstatic_double", 168),
    ("putstatic_object", 176),
    ("checkcast", 184),
    ("instanceof_check", 192),
    ("throw_aioobe", 200),
    ("throw_arithmetic", 208),
    ("invoke_dispatch", 216),
    ("invoke_virtual_mic", 224),
    ("lambda_int_to_double", 232),
    ("write_barrier", 240),
    ("satb_pre_write_barrier", 248),
    ("uncommon_trap", 256),
    ("math_fma_double", 264),
    ("math_fma_float", 272),
    ("tlab_cursor_offset_in_thread", 280),
    ("tlab_end_offset_in_thread", 288),
    ("class_id_offset_in_obj", 296),
    ("get_current_thread", 304),
    ("tlab_post_init", 312),
    ("frame_record", 320),
    ("shadow_stack_offset_in_thread", 328),
    ("throw_exception", 336),
    ("jit_npe_with_action", 344),
    ("dispatch_threw", 352),
    ("jit_frem", 360),
    ("jit_drem", 368),
    ("self_call_stack_guard", 376),
    ("region_bounds_addr", 384),
    ("native_stack_floor_fn", 392),
    ("ldc_string", 400),
    ("safepoint_flag_addr", 408),
    ("safepoint_slow_path", 416),
    ("jit_card_table_addr", 424),
    ("jit_card_old_base", 432),
    ("jit_card_old_end", 440),
    ("set_throw_bci", 448),
    ("service_callee_deopt", 456),
    ("new_object_cp", 464),
    ("anewarray_object_cp", 472),
    ("monitor_enter", 480),
    ("monitor_exit", 488),
    ("ldc_class_cp", 496),
    ("aastore_type_check", 504),
    ("read_bounds_addr", 512),
    ("local_handler_lookup", 520),
    ("ldc_string_cp", 528),
    ("ffm_segment_get", 536),
    ("ffm_segment_set", 544),
    ("ref_store_pre_gate", 552),
    ("ref_store_post_gate", 560),
    ("ref_store_post_young_floor", 568),
    ("g1_barrier_addr", 576),
    ("g1_post_write_barrier", 584),
    ("tlab_registration_required", 592),
];

// Every golden row must name the descriptor row at the same index AND agree
// with its `offset_of!`-derived offset.
//
// TRIPS ON: renaming a field; inserting a field anywhere but the end;
// swapping two fields (even two same-width `OptionalPtr`s, even if the
// descriptor table and the alias list are swapped in step); deleting a field.
const _: () = {
    let mut i = 0;
    while i < NUM_HELPER_FIELDS {
        assert!(
            str_eq(GOLDEN_HELPER_OFFSETS[i].0, HELPER_FIELDS[i].name),
            "GOLDEN_HELPER_OFFSETS disagrees with HELPER_FIELDS on the name of \
             a slot — a field was renamed, reordered, inserted or removed. The \
             golden table is the frozen contract; do not edit it to match.",
        );
        assert!(
            GOLDEN_HELPER_OFFSETS[i].1 == HELPER_FIELDS[i].offset,
            "a JitRuntimeHelpers field is no longer at its golden byte offset \
             — the layout moved. Do not edit the golden number; move the field \
             back and append instead.",
        );
        assert!(
            GOLDEN_HELPER_OFFSETS[i].1 == i * HELPER_FIELD_STRIDE,
            "the golden offset table is not a dense 8-byte-strided sequence — \
             a row was inserted, removed, or given a hand-typed wrong offset",
        );
        i += 1;
    }
};

// ---------------------------------------------------------------------
// The ABI revision ledger.
//
// `JIT_HELPERS_ABI_VERSION` is the one invariant the previous wave got wrong:
// `monitor_enter`/`monitor_exit` were appended (60 fields/480 bytes → 62/496),
// every size and count literal was updated deliberately, and the version stayed
// at 2 — because nothing connected the two. The contract at the top of this
// module says a bump is mandatory "including a pure append"; a contract with no
// tripwire is a comment.
//
// The ledger connects them: the last row must describe the table as it is now.
// ---------------------------------------------------------------------

/// One historical shape of the helper table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelperAbiRevision {
    /// The [`JIT_HELPERS_ABI_VERSION`] value that named this shape.
    pub version: u32,
    /// How many slots the table had.
    pub num_fields: usize,
    /// `size_of::<JitRuntimeHelpers>()` at that revision.
    pub size: usize,
}

/// Every shape [`JitRuntimeHelpers`] has had, oldest first.
///
/// Append a row whenever the table's shape changes, and bump
/// [`JIT_HELPERS_ABI_VERSION`] to the new row's `version`. The const assertions
/// below make that the only way to get a shape change to compile.
pub const ABI_REVISIONS: &[HelperAbiRevision] = &[
    // v1 — the table before the constant-pool-indexed allocation helpers.
    HelperAbiRevision {
        version: 1,
        num_fields: 58,
        size: 464,
    },
    // v2 — appended `new_object_cp`, `anewarray_object_cp`.
    HelperAbiRevision {
        version: 2,
        num_fields: 60,
        size: 480,
    },
    // v3 — appended `monitor_enter`, `monitor_exit`. (Shipped under v2 by
    // mistake; the ledger is what makes that mistake unrepresentable.)
    HelperAbiRevision {
        version: 3,
        num_fields: 62,
        size: 496,
    },
    // v4 — appended `ldc_class_cp`, so that an `ldc <Class>` compiles at all.
    HelperAbiRevision {
        version: 4,
        num_fields: 63,
        size: 504,
    },
    // v5 — appended `aastore_type_check`, so that a JIT-compiled `aastore`
    // throws `ArrayStoreException` at all. The x64 emitter lowers `aastore`
    // inline and therefore never calls `aastore`; the covariance check is the
    // one part of that helper the inline path cannot do for itself.
    HelperAbiRevision {
        version: 5,
        num_fields: 64,
        size: 512,
    },
    // v6 -- appended `read_bounds_addr`, the READ-side sibling of
    // `region_bounds_addr`. The older table cannot answer both questions: since
    // G1-2 its EMPTINESS is what keeps inline reference stores unreachable under
    // G1/ZGC, so filling it to make inline field reads reachable would unblock
    // the stores it exists to block. Two tables, one question each.
    HelperAbiRevision {
        version: 6,
        num_fields: 65,
        size: 520,
    },
    // v7 -- appended `local_handler_lookup`, which is what lets a compiled
    // frame enter its OWN `catch` block instead of deopting out to run it
    // interpreted. Optional: a zero slot arms no local-handler stubs at all.
    HelperAbiRevision {
        version: 7,
        num_fields: 66,
        size: 528,
    },
    // v8 -- appended `ldc_string_cp`, the CP-INDEXED form of the string `ldc`.
    // `ldc_string` bakes the literal's bytes and so has no key to answer a
    // recorded resolution with; it re-derived the constant on every execution
    // (the string pool's lock, a hash of the whole literal, a `memcmp`) where
    // JVMS §5.4.3 says a constant-pool entry resolves ONCE. The bytes form
    // stays in the table -- the ABI is append-only -- and is no longer emitted.
    HelperAbiRevision {
        version: 8,
        num_fields: 67,
        size: 536,
    },
    // v9 -- appended `ffm_segment_get` / `ffm_segment_set`, the FFM ELEMENT
    // accessor fast paths. `MemorySegment.getAtIndex`/`setAtIndex` are driven
    // one element at a time by any segment-backed array and measured ~1158
    // ns/element through the ordinary dispatch funnel, against ~0.8 ns for a
    // `short[]` element. Optional: a zero slot emits no fast path and every
    // site keeps the native dispatch it has today.
    HelperAbiRevision {
        version: 9,
        num_fields: 69,
        size: 552,
    },
    // v10 -- appended the three REFERENCE-STORE BARRIER GATES. Each is the
    // address of a collector-owned byte that names a PREFIX of a barrier
    // helper's own control flow, so compiled code can skip the CALL exactly
    // when the helper would have returned on its first test. They replace an
    // inference the emitter was making from `region_bounds_addr`, whose
    // emptiness under G1 and ZGC left every reference store paying six
    // containment compares that could never pass and then calling the helper
    // anyway. Optional: all-zero is "no plan published" and restores that
    // helper path exactly.
    HelperAbiRevision {
        version: 10,
        num_fields: 72,
        size: 576,
    },
    // v11 -- appended `g1_barrier_addr` / `g1_post_write_barrier` (F-08), the
    // table and the call target G1's INLINE post-write barrier needs. Closing
    // defect G1-2 had made every JIT-compiled reference store an out-of-line
    // `putfield_object` call under G1, because the inline arms are gated on a
    // table G1 deliberately leaves empty; this pair is what lets the emitter
    // put a real G1 barrier inline instead of borrowing the generational one's
    // premises. Both optional: zeros emit no inline barrier and every store
    // keeps the helper call it takes today.
    //
    // Appended AFTER v10's three gates rather than beside them: both landed on
    // 2026-09-02 in parallel branches, and v10 reached `dev` first, so its
    // offsets are the established ones and these two go on the end. That is the
    // whole reason this is v11 and not a second v10.
    HelperAbiRevision {
        version: 11,
        num_fields: 74,
        size: 592,
    },
    // v12 -- appended `tlab_registration_required`. ZGC's object-start registry
    // is its only record that an object exists, and the single-pass inline
    // TLAB `new` has a fast path that skips `tlab_post_init` -- the helper that
    // registers it. That path had been unreachable under ZGC because
    // `VmHeap::refill_tlab` answered `None` there, so a thread TLAB was always
    // empty; giving mutators a chunk made it reachable and, with it, live
    // objects invisible to the sweep, to `is_object_address` and to the
    // conservative root scan. Optional: `0` is every previous backend's
    // emission byte for byte.
    HelperAbiRevision {
        version: 12,
        num_fields: 75,
        size: 600,
    },
];

// The newest ledger row must be exactly the table this build compiles.
//
// TRIPS ON: appending a helper field (the row's `num_fields`/`size` no longer
// match) without adding a ledger row and bumping JIT_HELPERS_ABI_VERSION. Also
// trips if the version is bumped without a matching row, or a row is added
// without bumping the version.
const _: () = {
    let last = ABI_REVISIONS[ABI_REVISIONS.len() - 1];
    assert!(
        last.version == JIT_HELPERS_ABI_VERSION,
        "JIT_HELPERS_ABI_VERSION is not the newest ABI_REVISIONS row — a shape \
         change must append a ledger row AND bump the version constant",
    );
    assert!(
        last.num_fields == NUM_HELPER_FIELDS,
        "the helper table's field count changed without a new ABI_REVISIONS row \
         — append one and bump JIT_HELPERS_ABI_VERSION (this is exactly the \
         check the monitor_enter/monitor_exit append walked through)",
    );
    assert!(
        last.size == JIT_HELPERS_ABI_SIZE,
        "the helper table's size changed without a new ABI_REVISIONS row",
    );
};

// The ledger itself must describe an append-only history: versions increase by
// one, field counts strictly increase, and size stays `fields * 8`.
//
// TRIPS ON: a ledger row that records a *removal* or a reorder — i.e. writing
// down a change the "append only" rule forbids, rather than quietly making it.
const _: () = {
    let mut i = 0;
    while i < ABI_REVISIONS.len() {
        let r = ABI_REVISIONS[i];
        assert!(
            r.version as usize == i + 1,
            "ABI_REVISIONS versions must be dense and start at 1",
        );
        assert!(
            r.size == r.num_fields * HELPER_FIELD_STRIDE,
            "an ABI_REVISIONS row records a size that is not fields * 8 — the \
             table would have gained padding or a non-usize field",
        );
        if i > 0 {
            assert!(
                r.num_fields > ABI_REVISIONS[i - 1].num_fields,
                "an ABI_REVISIONS row does not grow the table — the helper \
                 table is append-only; removing a slot rebases every later \
                 offset and is not a supported edit",
            );
        }
        i += 1;
    }
};

// ---------------------------------------------------------------------
// Signature pins.
//
// `HELPER_FN_SIGS` is derived from the `HelperFn*` aliases, so these assert
// facts about the *shape* of the helper ABI that the emitter's hand-written
// call sites depend on.
// ---------------------------------------------------------------------

// Every callable descriptor row has exactly one signature row with the same
// name, and vice versa. The pre-existing check compared only COUNTS, which a
// pair of compensating edits (drop one alias, add another) satisfies.
//
// TRIPS ON: adding a field to `helper_field_table!` as `Function` without
// adding its row to `helper_fn_slots!`; renaming a field in one list only.
const _: () = {
    let mut s = 0;
    while s < HELPER_FN_SIGS.len() {
        let mut found = 0;
        let mut d = 0;
        while d < NUM_HELPER_FIELDS {
            if str_eq(HELPER_FN_SIGS[s].field, HELPER_FIELDS[d].name) {
                assert!(
                    matches!(HELPER_FIELDS[d].kind, HelperKind::Function),
                    "a slot has a callable signature but is described as a \
                     non-callable (Offset/Constant) slot",
                );
                found += 1;
            }
            d += 1;
        }
        assert!(
            found == 1,
            "a `helper_fn_slots!` row names a field that has no (or more than \
             one) matching row in `helper_field_table!`",
        );
        s += 1;
    }
    let mut d = 0;
    while d < NUM_HELPER_FIELDS {
        if matches!(HELPER_FIELDS[d].kind, HelperKind::Function) {
            let mut found = 0;
            let mut s = 0;
            while s < HELPER_FN_SIGS.len() {
                if str_eq(HELPER_FN_SIGS[s].field, HELPER_FIELDS[d].name) {
                    found += 1;
                }
                s += 1;
            }
            assert!(
                found == 1,
                "a callable slot has no signature alias — add it to \
                 `helper_fn_slots!` so its arity and widths are pinned",
            );
        }
        d += 1;
    }
};

/// How many helpers take more arguments than the Win64 integer register file
/// holds, and therefore need the call site to write arguments to the stack
/// above the shadow space.
///
/// Exactly one today: `invoke_virtual_mic` (6 arguments; the MIC and PIC
/// pointers go to `[RSP+32]` and `[RSP+40]` on Windows and to R8/R9 on SysV —
/// `jit/src/x64.rs`, the `#[cfg(target_os = "windows")]` split at the
/// `invoke_virtual_mic` call site).
pub const HELPERS_NEEDING_WIN64_STACK_ARGS: usize = {
    let mut n = 0;
    let mut i = 0;
    while i < HELPER_FN_SIGS.len() {
        if HELPER_FN_SIGS[i].win64_stack_args() > 0 {
            n += 1;
        }
        i += 1;
    }
    n
};

// Arity and calling-convention shape.
//
// TRIPS ON (in order): a helper with more than 6 arguments; a helper mixing
// integer and floating arguments; a float helper with more than 4 arguments;
// a SECOND helper that spills arguments to the stack on Win64.
const _: () = {
    let mut i = 0;
    while i < HELPER_FN_SIGS.len() {
        let s = HELPER_FN_SIGS[i];
        assert!(
            s.arity <= SYSV_INT_ARG_REGS,
            "a helper takes more arguments than the SysV integer argument \
             register file holds; the emitter has no general stack-argument \
             path for helper calls",
        );
        // Win64 assigns the integer and SSE register files POSITIONALLY (arg 2
        // is RDX or XMM1 depending on its own type); SysV assigns them
        // independently (the first float is always XMM0). A helper that mixes
        // the two classes therefore needs a different register assignment per
        // platform, and every helper call site in the backend is written as if
        // one uniform rule applies. Today every helper is all-integer or
        // all-float, so the divergence is unreachable. Keep it that way.
        assert!(
            s.float_args == 0 || s.float_args == s.arity,
            "a helper mixes integer and floating-point arguments — Win64 \
             assigns argument registers positionally across both files and \
             SysV does not, so this helper needs a per-platform register \
             assignment the emitter does not have",
        );
        // Win64 has XMM0-XMM3 for float arguments.
        assert!(
            s.float_args <= WIN64_INT_ARG_REGS,
            "a floating-point helper takes more arguments than Win64's four \
             XMM argument registers",
        );
        i += 1;
    }
    assert!(
        HELPERS_NEEDING_WIN64_STACK_ARGS == 1,
        "the number of helpers needing Win64 stack arguments changed. Only \
         `invoke_virtual_mic` has ever exceeded four arguments, and its call \
         site in jit/src/x64.rs writes args 5 and 6 to [RSP+32]/[RSP+40] by \
         hand. A new >4-argument helper needs the same treatment; adding one \
         without it silently passes garbage in R8/R9 on Windows.",
    );
};

// Census pins. The runtime test in the crate root checks these too, but a
// `#[test]` only fires if someone runs it, and a reclassification is exactly
// the kind of edit that gets made without running this crate's tests.
//
// TRIPS ON: promoting or demoting any slot between Function/Offset/Constant,
// or between required and optional.
const _: () = {
    let (mut functions, mut offsets, mut constants) = (0usize, 0usize, 0usize);
    let (mut required, mut optional_fns) = (0usize, 0usize);
    let mut i = 0;
    while i < NUM_HELPER_FIELDS {
        match HELPER_FIELDS[i].kind {
            HelperKind::Function => {
                functions += 1;
                if HELPER_FIELDS[i].required {
                    required += 1;
                } else {
                    optional_fns += 1;
                }
            }
            HelperKind::Offset => offsets += 1,
            HelperKind::Constant => constants += 1,
        }
        i += 1;
    }
    assert!(functions == 60, "callable-slot count changed");
    assert!(
        offsets == 4,
        "the number of displacement slots changed — an Offset slot is baked as \
         a disp32 into an addressing mode, not CALLed; confirm the new one \
         really is a displacement and is range-checked by validate_with",
    );
    assert!(
        constants == 11,
        "the number of baked-address slots changed — a Constant slot is loaded \
         as data and is NOT range-checked by validate_with, so misclassifying \
         a displacement as one silently removes its only sanity check",
    );
    assert!(required == 43, "required-slot count changed");
    // 13 -> 12 on 2026-08-16, merging `dev`: `aastore_type_check` was promoted
    // from optional to required, so an optional slot LEFT the set. The check
    // this guard exists to force -- "does the new optional slot have a zero
    // check at its emitter call site?" -- has no subject when the count goes
    // DOWN, and the twelve that remain kept the zero checks they already had.
    // The runtime test below (`functions - required == 12`) was already on
    // the new number; this const was the only site still carrying 13.
    assert!(
        optional_fns == 17,
        "the optional-callable count changed — every optional slot MUST have a \
         zero check at its emitter call site; confirm the new one does before \
         updating this number",
    );
    assert!(functions + offsets + constants == NUM_HELPER_FIELDS);
    assert!(required + optional_fns == functions);
};

// ---------------------------------------------------------------------
// Platform pins.
//
// The JIT is 64-bit only and the accessors transmute a `usize` into a function
// pointer. `core::mem::transmute` already refuses a size mismatch, but it does
// so at the transmute, deep inside a macro expansion; these say why.
// ---------------------------------------------------------------------

// TRIPS ON: building for a 32-bit target. Every offset in the golden table
// above is `index * 8`.
const _: () = assert!(
    core::mem::size_of::<usize>() == 8 && core::mem::align_of::<usize>() == 8,
    "the helper ABI requires an 8-byte, 8-byte-aligned usize; the golden \
     offsets are `index * 8` and would silently halve on a 32-bit target",
);

// TRIPS ON: a target where a function pointer is not a plain machine word
// (a segmented or descriptor-based ABI). The `<field>_fn` accessors transmute
// `usize` → fn pointer, and `build_helpers` casts fn → usize.
const _: () = assert!(
    core::mem::size_of::<unsafe extern "C" fn()>() == core::mem::size_of::<usize>(),
    "a function pointer is not one machine word on this target — the helper \
     table stores entry points as bare `usize` and could not round-trip",
);

// TRIPS ON: a target without the null-pointer optimization for fn pointers.
// The accessors return `Option<HelperFn*>` and the whole optional-slot
// discipline rests on "zero means None"; if `Option` grew a discriminant word
// the mapping would still be correct but the zero-cost claim would be false.
const _: () = assert!(
    core::mem::size_of::<Option<unsafe extern "C" fn()>>() == core::mem::size_of::<usize>(),
    "Option<fn pointer> is not niche-optimized on this target",
);

// TRIPS ON: a raw pointer that is not a plain address (a fat or tagged
// pointer target). `*const u8` appears in three helper signatures.
const _: () = assert!(
    core::mem::size_of::<*const u8>() == core::mem::size_of::<usize>(),
    "a thin raw pointer is not one machine word on this target",
);

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

/// Bind a helper function to its declared [`JitRuntimeHelpers`] signature and
/// yield its address as the `usize` the table stores.
///
/// The producer (`vm/src/jit/helpers.rs::build_helpers`) currently writes
/// `jit_monitor_enter as *const () as usize`, which discards the signature: any
/// `extern "C"` function of any shape casts cleanly, so the arity and widths
/// declared in `helper_fn_slots!` are checked against *nothing*. Going through
/// this macro makes the coercion to the declared `HelperFn*` alias a compile
/// error when they disagree.
///
/// ```
/// use cratonvm_jit_api::typed_helper_addr;
///
/// unsafe extern "C" fn my_arraylength(_array_ptr: i64) -> i64 {
///     0
/// }
/// let slot: usize = typed_helper_addr!(HelperFnArraylength, my_arraylength);
/// assert_eq!(slot, my_arraylength as usize);
/// ```
///
/// Giving `my_arraylength` a second parameter, or a `-> ()` return, stops the
/// example compiling — which is the entire point.
#[macro_export]
macro_rules! typed_helper_addr {
    ($alias:ident, $f:expr $(,)?) => {{
        // The coercion is the check: a fn item only coerces to this fn-pointer
        // type if its `extern`-ness, arity, argument types and return type all
        // match the declared helper signature.
        const SIGNATURE_CHECKED: $crate::helpers_abi::$alias = $f;
        SIGNATURE_CHECKED as usize
    }};
}

/// Look up a slot descriptor by field name.
///
/// Linear over [`NUM_HELPER_FIELDS`] entries; intended for diagnostics and
/// tooling, not for a hot path.
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
            (
                "satb_pre_write_barrier",
                offset_of!(H, satb_pre_write_barrier),
            ),
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
            (
                "class_id_offset_in_obj",
                offset_of!(H, class_id_offset_in_obj),
            ),
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
            (
                "self_call_stack_guard",
                offset_of!(H, self_call_stack_guard),
            ),
            ("region_bounds_addr", offset_of!(H, region_bounds_addr)),
            (
                "native_stack_floor_fn",
                offset_of!(H, native_stack_floor_fn),
            ),
            ("ldc_string", offset_of!(H, ldc_string)),
            ("safepoint_flag_addr", offset_of!(H, safepoint_flag_addr)),
            ("safepoint_slow_path", offset_of!(H, safepoint_slow_path)),
            ("jit_card_table_addr", offset_of!(H, jit_card_table_addr)),
            ("jit_card_old_base", offset_of!(H, jit_card_old_base)),
            ("jit_card_old_end", offset_of!(H, jit_card_old_end)),
            ("set_throw_bci", offset_of!(H, set_throw_bci)),
            ("service_callee_deopt", offset_of!(H, service_callee_deopt)),
            ("new_object_cp", offset_of!(H, new_object_cp)),
            ("anewarray_object_cp", offset_of!(H, anewarray_object_cp)),
            ("monitor_enter", offset_of!(H, monitor_enter)),
            ("monitor_exit", offset_of!(H, monitor_exit)),
            ("ldc_class_cp", offset_of!(H, ldc_class_cp)),
            ("aastore_type_check", offset_of!(H, aastore_type_check)),
            ("read_bounds_addr", offset_of!(H, read_bounds_addr)),
            ("local_handler_lookup", offset_of!(H, local_handler_lookup)),
            ("ldc_string_cp", offset_of!(H, ldc_string_cp)),
            ("ffm_segment_get", offset_of!(H, ffm_segment_get)),
            ("ffm_segment_set", offset_of!(H, ffm_segment_set)),
            ("ref_store_pre_gate", offset_of!(H, ref_store_pre_gate)),
            ("ref_store_post_gate", offset_of!(H, ref_store_post_gate)),
            (
                "ref_store_post_young_floor",
                offset_of!(H, ref_store_post_young_floor),
            ),
            ("g1_barrier_addr", offset_of!(H, g1_barrier_addr)),
            ("g1_post_write_barrier", offset_of!(H, g1_post_write_barrier)),
            (
                "tlab_registration_required",
                offset_of!(H, tlab_registration_required),
            ),
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
        assert_eq!(core::mem::size_of::<H>(), 600);
        assert_eq!(core::mem::align_of::<H>(), 8);
        assert_eq!(JIT_HELPERS_ABI_SIZE, 600);
        assert_eq!(JIT_HELPERS_ABI_ALIGN, 8);
        assert_eq!(HELPER_FIELD_STRIDE, 8);
        assert_eq!(NUM_HELPER_FIELDS, 75);
        assert_eq!(H::NUM_FIELDS, 75);
        // v10's three appended slots are gate ADDRESSES, not call targets, so
        // the callable-slot count stood still while the table grew. v11 (F-08)
        // appended one of each: `g1_barrier_addr` is a table address and
        // `g1_post_write_barrier` IS a call target, so the callable count moves
        // by exactly one. That divergence is the point of counting them apart.
        // v12 appended `tlab_registration_required`, a 0/1 FLAG rather than a
        // call target, so the callable count stands still again.
        assert_eq!(H::NUM_HELPER_FN_FIELDS, 60);
        assert_eq!(JIT_HELPERS_ABI_VERSION, 12);
    }

    /// The golden table is the only name→offset binding in the crate written
    /// as literals. Everything else derives from the struct and therefore
    /// follows a reorder instead of catching it.
    #[test]
    fn golden_offsets_are_the_struct_offsets() {
        assert_eq!(GOLDEN_HELPER_OFFSETS.len(), H::NUM_FIELDS);
        for (i, (name, offset)) in GOLDEN_HELPER_OFFSETS.iter().enumerate() {
            let desc = helper_field(name)
                .unwrap_or_else(|| panic!("golden table names an unknown slot `{}`", name));
            assert_eq!(
                desc.offset, *offset,
                "`{}` moved: golden offset {}, struct offset {}",
                name, offset, desc.offset,
            );
            assert_eq!(
                HELPER_FIELDS[i].name, *name,
                "golden row {} is `{}`, HELPER_FIELDS row {} is `{}`",
                i, name, i, HELPER_FIELDS[i].name,
            );
        }
        // The last golden offset plus one stride is the whole table.
        let (last_name, last_offset) = GOLDEN_HELPER_OFFSETS[H::NUM_FIELDS - 1];
        assert_eq!(last_name, "tlab_registration_required");
        assert_eq!(last_offset + HELPER_FIELD_STRIDE, JIT_HELPERS_ABI_SIZE);
    }

    #[test]
    fn abi_revision_ledger_names_the_current_table() {
        let last = *ABI_REVISIONS.last().expect("the ledger is never empty");
        assert_eq!(last.version, JIT_HELPERS_ABI_VERSION);
        assert_eq!(last.num_fields, NUM_HELPER_FIELDS);
        assert_eq!(last.size, core::mem::size_of::<H>());
        assert_eq!(
            last,
            HelperAbiRevision {
                version: 12,
                num_fields: 75,
                size: 600,
            },
        );
        // Append-only history: each revision strictly grows the table.
        for pair in ABI_REVISIONS.windows(2) {
            assert!(pair[1].version == pair[0].version + 1);
            assert!(pair[1].num_fields > pair[0].num_fields);
        }
    }

    /// The signature table is derived from the `HelperFn*` aliases, so this
    /// pins the shapes the emitter's hand-written call sites assume.
    #[test]
    fn helper_fn_sigs_record_the_real_c_signatures() {
        let by_name = |n: &str| {
            *HELPER_FN_SIGS
                .iter()
                .find(|s| s.field == n)
                .unwrap_or_else(|| panic!("no signature row for `{}`", n))
        };

        assert_eq!(HELPER_FN_SIGS.len(), H::NUM_HELPER_FN_FIELDS);

        // The only helper that outruns Win64's four integer argument
        // registers. Its call site writes args 5 and 6 to [RSP+32]/[RSP+40].
        let mic = by_name("invoke_virtual_mic");
        assert_eq!((mic.arity, mic.float_args), (6, 0));
        assert_eq!(mic.win64_stack_args(), 2);
        assert_eq!(mic.int_args(), 6);
        assert!(mic.returns_value && !mic.returns_float);
        assert_eq!(HELPERS_NEEDING_WIN64_STACK_ARGS, 1);

        // All-float helpers: operands and result in XMM.
        let fma = by_name("math_fma_double");
        assert_eq!((fma.arity, fma.float_args), (3, 3));
        assert!(fma.returns_value && fma.returns_float);
        let frem = by_name("jit_frem");
        assert_eq!((frem.arity, frem.float_args), (2, 2));
        assert!(frem.returns_float);

        // `-> ()`. A call site that treats one of these as returning a value
        // reads whatever the callee left in RAX.
        for void_helper in [
            "jit_npe_with_action",
            "safepoint_slow_path",
            "set_throw_bci",
            "write_barrier",
            "frame_record",
        ] {
            assert!(
                !by_name(void_helper).returns_value,
                "`{}` is declared `-> ()`",
                void_helper,
            );
        }

        // Zero-argument helpers.
        for nullary in [
            "throw_arithmetic",
            "dispatch_threw",
            "get_current_thread",
            "safepoint_slow_path",
            "native_stack_floor_fn",
        ] {
            assert_eq!(
                by_name(nullary).arity,
                0,
                "`{}` takes no arguments",
                nullary
            );
        }

        // The mechanical accessor-naming rule, including the field that
        // already ends in `_fn`.
        for sig in HELPER_FN_SIGS {
            assert_eq!(
                sig.accessor,
                format!("{}_fn", sig.field),
                "accessor naming rule broken for `{}`",
                sig.field,
            );
            assert!(accessor_name_matches_field(sig.accessor, sig.field));
            assert!(sig.float_args == 0 || sig.float_args == sig.arity);
        }
        assert_eq!(
            by_name("native_stack_floor_fn").accessor,
            "native_stack_floor_fn_fn",
        );
    }

    #[test]
    fn every_accessor_is_none_on_a_zeroed_table() {
        let h = H::default();
        let probe = h.helper_fn_accessor_probe();
        assert_eq!(probe.len(), H::NUM_HELPER_FN_FIELDS);
        for (name, value) in probe.iter() {
            assert!(
                value.is_none(),
                "`{}_fn` returned Some for an unwired (zero) slot — a null \
                 helper could be CALLed",
                name,
            );
        }
    }

    /// Wire exactly one callable slot and require that exactly one accessor
    /// sees it, and that it is the accessor named for that slot.
    ///
    /// This is what catches two swapped rows in `helper_fn_slots!`: swapping
    /// the fields of two rows leaves every count, offset, census and size
    /// check satisfied while handing each helper the other's C signature.
    #[test]
    fn accessor_reads_only_its_own_slot() {
        unsafe extern "C" fn marker() {}
        let addr = marker as usize;
        assert_ne!(addr, 0);

        let fn_slots: Vec<&HelperFieldDesc> = HELPER_FIELDS
            .iter()
            .filter(|d| d.kind == HelperKind::Function)
            .collect();
        assert_eq!(fn_slots.len(), H::NUM_HELPER_FN_FIELDS);

        for target in fn_slots {
            let mut h = H::default();
            let idx = target.offset / HELPER_FIELD_STRIDE;
            // SAFETY: `idx < NUM_FIELDS` because `offset` came from
            // `offset_of!` on this struct and the layout is exactly
            // `[usize; NUM_FIELDS]` (see `as_words`). The write is in bounds,
            // correctly typed and correctly aligned. `addr` is the address of
            // a real function, so no accessor fabricates an invalid function
            // pointer; none of them are called.
            unsafe { *(&mut h as *mut H as *mut usize).add(idx) = addr };

            let probe = h.helper_fn_accessor_probe();
            let wired: Vec<&'static str> = probe
                .iter()
                .filter(|(_, v)| v.is_some())
                .map(|(n, _)| *n)
                .collect();
            assert_eq!(
                wired,
                vec![target.name],
                "wiring only `{}` should light up only its own accessor",
                target.name,
            );
            let (_, value) = probe
                .iter()
                .find(|(n, _)| *n == target.name)
                .expect("the probe covers every callable slot");
            assert_eq!(
                *value,
                Some(addr),
                "`{}_fn` returned a different address than the slot holds",
                target.name,
            );
        }
    }

    #[test]
    fn typed_helper_addr_yields_the_declared_functions_address() {
        unsafe extern "C" fn stub_arraylength(_array_ptr: i64) -> i64 {
            3
        }
        // Compiles only because `stub_arraylength` really has
        // `HelperFnArraylength`'s shape.
        let addr = crate::typed_helper_addr!(HelperFnArraylength, stub_arraylength);
        assert_eq!(addr, stub_arraylength as usize);

        let mut h = H::default();
        h.arraylength = addr;
        let f = h.arraylength_fn().expect("wired slot must be Some");
        // SAFETY: the slot holds `stub_arraylength`, which matches
        // `HelperFnArraylength` exactly and dereferences none of its arguments.
        assert_eq!(unsafe { f(0) }, 3);
    }

    /// The census the const assertions pin, restated as values so a failure
    /// says which class drifted rather than only that one did.
    #[test]
    fn slot_census_matches_the_pinned_counts() {
        let functions = HELPER_FIELDS
            .iter()
            .filter(|d| d.kind == HelperKind::Function)
            .count();
        let offsets = HELPER_FIELDS
            .iter()
            .filter(|d| d.kind == HelperKind::Offset)
            .count();
        let constants = HELPER_FIELDS
            .iter()
            .filter(|d| d.kind == HelperKind::Constant)
            .count();
        let required = HELPER_FIELDS.iter().filter(|d| d.required).count();
        assert_eq!(functions, 60, "callable slots");
        assert_eq!(offsets, 4, "displacement slots");
        // v10 appended three Constants (gate ADDRESSES, not call targets) and
        // v11 (F-08) appended one more Constant — `g1_barrier_addr`, the
        // geometry table's address — plus one optional Function,
        // `g1_post_write_barrier`. So the callable count moved by exactly one
        // across the two revisions and the baked-address count by four, which
        // is the divergence counting them apart exists to show.
        assert_eq!(constants, 11, "baked-address slots");
        assert_eq!(required, 43, "required slots");
        assert_eq!(functions - required, 17, "optional callable slots");
        assert_eq!(functions + offsets + constants, H::NUM_FIELDS);
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
        assert_eq!(
            h.validate_abi(),
            Err(HelperAbiError::MissingRequired("newarray"))
        );
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
        // The LAST field, whatever it currently is — `tlab_registration_required`
        // since v12 appended the flag that keeps an inline-TLAB `new` from
        // skipping the helper that REGISTERS the object on a collector whose
        // object-start registry is its only record of it.
        h.tlab_registration_required = 2;
        let w = h.as_words();
        assert_eq!(w[0], 1, "first slot");
        assert_eq!(w[H::NUM_FIELDS - 1], 2, "last slot");
        assert_eq!(w.len(), 75);
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
