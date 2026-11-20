// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! VarHandle API implementation (java.lang.invoke.VarHandle, Java 9+).
//!
//! Provides typed access to variables with memory ordering semantics,
//! replacing `sun.misc.Unsafe` for most use cases. Supports instance fields,
//! static fields, array elements, and ByteBuffer views with full CAS and
//! read-modify-write atomic operations.

use std::collections::HashMap;
use std::sync::atomic::{fence, AtomicI32, AtomicI64, Ordering};

// ---------------------------------------------------------------------------
// AccessMode
// ---------------------------------------------------------------------------

/// All access modes defined by `java.lang.invoke.VarHandle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccessMode {
    // -- Read --
    Get,
    GetVolatile,
    GetAcquire,
    GetOpaque,

    // -- Write --
    Set,
    SetVolatile,
    SetRelease,
    SetOpaque,

    // -- Compare-and-set --
    CompareAndSet,
    CompareAndExchangeVolatile,
    CompareAndExchangeAcquire,
    CompareAndExchangeRelease,
    WeakCompareAndSetPlain,
    WeakCompareAndSet,
    WeakCompareAndSetAcquire,
    WeakCompareAndSetRelease,

    // -- Atomic read-modify-write --
    GetAndSet,
    GetAndSetAcquire,
    GetAndSetRelease,
    GetAndAdd,
    GetAndAddAcquire,
    GetAndAddRelease,
    GetAndBitwiseOr,
    GetAndBitwiseOrAcquire,
    GetAndBitwiseOrRelease,
    GetAndBitwiseAnd,
    GetAndBitwiseAndAcquire,
    GetAndBitwiseAndRelease,
    GetAndBitwiseXor,
    GetAndBitwiseXorAcquire,
    GetAndBitwiseXorRelease,
}

/// Total number of `AccessMode` variants.
pub const ACCESS_MODE_COUNT: usize = 31;

impl AccessMode {
    /// Returns all access mode variants in declaration order.
    pub fn all() -> &'static [AccessMode] {
        use AccessMode::*;
        &[
            Get,
            GetVolatile,
            GetAcquire,
            GetOpaque,
            Set,
            SetVolatile,
            SetRelease,
            SetOpaque,
            CompareAndSet,
            CompareAndExchangeVolatile,
            CompareAndExchangeAcquire,
            CompareAndExchangeRelease,
            WeakCompareAndSetPlain,
            WeakCompareAndSet,
            WeakCompareAndSetAcquire,
            WeakCompareAndSetRelease,
            GetAndSet,
            GetAndSetAcquire,
            GetAndSetRelease,
            GetAndAdd,
            GetAndAddAcquire,
            GetAndAddRelease,
            GetAndBitwiseOr,
            GetAndBitwiseOrAcquire,
            GetAndBitwiseOrRelease,
            GetAndBitwiseAnd,
            GetAndBitwiseAndAcquire,
            GetAndBitwiseAndRelease,
            GetAndBitwiseXor,
            GetAndBitwiseXorAcquire,
            GetAndBitwiseXorRelease,
        ]
    }
}

// ---------------------------------------------------------------------------
// VarType
// ---------------------------------------------------------------------------

/// JVM variable types that a `VarHandle` can target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarType {
    Int,
    Long,
    Float,
    Double,
    Object,
    Byte,
    Short,
    Char,
    Boolean,
}

impl VarType {
    /// Returns all `VarType` variants.
    pub fn all() -> &'static [VarType] {
        use VarType::*;
        &[Int, Long, Float, Double, Object, Byte, Short, Char, Boolean]
    }

    /// Whether this type is numeric (supports GetAndAdd).
    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            VarType::Int | VarType::Long | VarType::Float | VarType::Double
        )
    }

    /// Whether this type is an integer type (supports bitwise ops).
    pub fn is_integer(&self) -> bool {
        matches!(
            self,
            VarType::Int | VarType::Long | VarType::Byte | VarType::Short | VarType::Char
        )
    }
}

// ---------------------------------------------------------------------------
// VarHandleKind
// ---------------------------------------------------------------------------

/// Describes what kind of variable a `VarHandle` references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VarHandleKind {
    /// Instance field: `obj.field`
    InstanceField {
        class_name: String,
        field_name: String,
        field_index: usize,
    },
    /// Static field: `Class.field`
    StaticField {
        class_name: String,
        field_name: String,
        field_index: usize,
    },
    /// Array element: `arr[i]`
    ArrayElement { element_type: VarType },
    /// ByteBuffer view: `buf.getInt(offset)`
    ByteBufferView { element_type: VarType },
}

// ---------------------------------------------------------------------------
// VarHandle
// ---------------------------------------------------------------------------

/// A variable handle providing typed, memory-ordered access to a variable.
pub struct VarHandle {
    pub kind: VarHandleKind,
    pub var_type: VarType,
    /// Which access modes are supported for this VarHandle.
    pub supported_modes: Vec<AccessMode>,
}

impl VarHandle {
    /// Build the set of supported access modes for a given `VarType`.
    pub fn supported_modes_for(var_type: VarType) -> Vec<AccessMode> {
        use AccessMode::*;

        // All types support basic read/write modes.
        let mut modes = vec![
            Get,
            GetVolatile,
            GetAcquire,
            GetOpaque,
            Set,
            SetVolatile,
            SetRelease,
            SetOpaque,
        ];

        let cas_modes: &[AccessMode] = &[
            CompareAndSet,
            CompareAndExchangeVolatile,
            CompareAndExchangeAcquire,
            CompareAndExchangeRelease,
            WeakCompareAndSetPlain,
            WeakCompareAndSet,
            WeakCompareAndSetAcquire,
            WeakCompareAndSetRelease,
        ];

        let get_and_set_modes: &[AccessMode] = &[GetAndSet, GetAndSetAcquire, GetAndSetRelease];

        let get_and_add_modes: &[AccessMode] = &[GetAndAdd, GetAndAddAcquire, GetAndAddRelease];

        let bitwise_modes: &[AccessMode] = &[
            GetAndBitwiseOr,
            GetAndBitwiseOrAcquire,
            GetAndBitwiseOrRelease,
            GetAndBitwiseAnd,
            GetAndBitwiseAndAcquire,
            GetAndBitwiseAndRelease,
            GetAndBitwiseXor,
            GetAndBitwiseXorAcquire,
            GetAndBitwiseXorRelease,
        ];

        match var_type {
            // Numeric types: CAS + GetAndSet + GetAndAdd
            VarType::Int | VarType::Long => {
                modes.extend_from_slice(cas_modes);
                modes.extend_from_slice(get_and_set_modes);
                modes.extend_from_slice(get_and_add_modes);
                // Integer types also get bitwise
                modes.extend_from_slice(bitwise_modes);
            }
            VarType::Float | VarType::Double => {
                modes.extend_from_slice(cas_modes);
                modes.extend_from_slice(get_and_set_modes);
                modes.extend_from_slice(get_and_add_modes);
            }
            VarType::Byte | VarType::Short | VarType::Char => {
                // Integer types: CAS + GetAndSet + GetAndAdd + bitwise
                modes.extend_from_slice(cas_modes);
                modes.extend_from_slice(get_and_set_modes);
                modes.extend_from_slice(get_and_add_modes);
                modes.extend_from_slice(bitwise_modes);
            }
            VarType::Object => {
                modes.extend_from_slice(cas_modes);
                modes.extend_from_slice(get_and_set_modes);
            }
            // HotSpot 25: a `boolean` variable has the bitwise atomic updates
            // (`getAndBitwiseOr` / `And` / `Xor` and their orderings) but no
            // numeric one (`VarHandleBooleans`, `X-VarHandle.java.template`'s
            // `#if[Bitwise]`; `isAccessModeSupported(GET_AND_BITWISE_OR)` is
            // `true` on `findVarHandle(C, "b", boolean.class)`). This model
            // listed none (interpreter round i6 wave 115, lane L4); the
            // registered native's rule (`lang_invoke::vh_access_mode_supported`)
            // already had it.
            VarType::Boolean => {
                modes.extend_from_slice(cas_modes);
                modes.extend_from_slice(get_and_set_modes);
                modes.extend_from_slice(bitwise_modes);
            }
        }

        modes
    }

    /// Returns `true` if the given access mode is supported by this handle.
    pub fn is_mode_supported(&self, mode: AccessMode) -> bool {
        self.supported_modes.contains(&mode)
    }
}

// ---------------------------------------------------------------------------
// MemoryOrdering
// ---------------------------------------------------------------------------

/// Maps Java memory ordering semantics to Rust `std::sync::atomic::Ordering`.
pub struct MemoryOrdering;

impl MemoryOrdering {
    /// Plain: no ordering guarantees (Relaxed).
    pub fn plain() -> Ordering {
        Ordering::Relaxed
    }

    /// Opaque: no reordering with other opaque ops on the same variable.
    /// Closest Rust equivalent is `Relaxed`.
    pub fn opaque() -> Ordering {
        Ordering::Relaxed
    }

    /// Acquire: reads after this see writes before the corresponding release.
    pub fn acquire() -> Ordering {
        Ordering::Acquire
    }

    /// Release: writes before this are visible after the corresponding acquire.
    pub fn release() -> Ordering {
        Ordering::Release
    }

    /// Volatile: sequential consistency.
    pub fn volatile() -> Ordering {
        Ordering::SeqCst
    }

    /// Returns the appropriate `Ordering` for a given `AccessMode`.
    pub fn for_access_mode(mode: AccessMode) -> Ordering {
        use AccessMode::*;
        match mode {
            // Plain
            Get | Set | WeakCompareAndSetPlain => Self::plain(),

            // Opaque
            GetOpaque | SetOpaque => Self::opaque(),

            // Acquire
            GetAcquire
            | CompareAndExchangeAcquire
            | WeakCompareAndSetAcquire
            | GetAndSetAcquire
            | GetAndAddAcquire
            | GetAndBitwiseOrAcquire
            | GetAndBitwiseAndAcquire
            | GetAndBitwiseXorAcquire => Self::acquire(),

            // Release
            SetRelease
            | CompareAndExchangeRelease
            | WeakCompareAndSetRelease
            | GetAndSetRelease
            | GetAndAddRelease
            | GetAndBitwiseOrRelease
            | GetAndBitwiseAndRelease
            | GetAndBitwiseXorRelease => Self::release(),

            // Volatile (SeqCst)
            GetVolatile
            | SetVolatile
            | CompareAndSet
            | CompareAndExchangeVolatile
            | WeakCompareAndSet
            | GetAndSet
            | GetAndAdd
            | GetAndBitwiseOr
            | GetAndBitwiseAnd
            | GetAndBitwiseXor => Self::volatile(),
        }
    }
}

// ---------------------------------------------------------------------------
// FenceOperations
// ---------------------------------------------------------------------------

/// Memory fence operations corresponding to `VarHandle` static methods and
/// `sun.misc.Unsafe` fence methods.
pub struct FenceOperations;

impl FenceOperations {
    /// Full memory fence (SeqCst barrier).
    /// All loads and stores before the fence are ordered before all loads and
    /// stores after the fence.
    pub fn full_fence() {
        fence(Ordering::SeqCst);
    }

    /// Acquire fence: loads after this see stores before a release fence.
    pub fn acquire_fence() {
        fence(Ordering::Acquire);
    }

    /// Release fence: stores before this are visible after an acquire fence.
    pub fn release_fence() {
        fence(Ordering::Release);
    }

    /// LoadLoad fence: loads before are ordered before loads after.
    /// On x86 this is a no-op; use Acquire for portability.
    pub fn load_load_fence() {
        fence(Ordering::Acquire);
    }

    /// StoreStore fence: stores before are ordered before stores after.
    /// Use Release for portability.
    pub fn store_store_fence() {
        fence(Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// AtomicOperations
// ---------------------------------------------------------------------------

/// **DO NOT USE FOR CONCURRENT FIELD UPDATES.**
///
/// These helpers were originally drafted as a sketch of the VarHandle /
/// `j.u.c.atomic.*` operation set. Each method constructs a *stack-local*
/// `AtomicI32`/`AtomicI64` from a snapshot of the heap value, performs the
/// atomic op on that local, and returns the result. The caller would then be
/// expected to write the value back to the heap — but that write is racy and
/// for compare-and-set semantics it is **silently wrong**: the CAS targets a
/// fresh local atomic that no other thread can see, so it always "succeeds"
/// against the snapshot and never publishes to the actual heap slot. A
/// concurrent CAS on the same field will not observe this op at all.
///
/// (Round-9 concurrency CRIT-3 — `compare_and_set_int/long` were latent UB
/// for any caller outside this file's own unit tests.) The
/// `get_and_{set,add,or,and,xor}_{int,long}` read-modify-write helpers share
/// the exact same defect: the RMW runs against a stack-local atomic, so the
/// result is computed-and-discarded and the heap field is never updated (lost
/// update). They are gated identically (see below).
///
/// The real heap-CAS path is `crate::native_api::NativeContext::compare_and_swap_value`
/// and the interpreter's `Putfield_Volatile` / `Unsafe.compareAndSet*` natives,
/// which take an `&AtomicI32` / `&AtomicI64` pointing into the actual heap slot.
/// Use those instead.
///
/// All entry points are now `#[deprecated]` and panic if invoked at runtime;
/// the in-file unit tests still call them under `#[allow(deprecated)]` because
/// they only exercise the local-snapshot algebra (not concurrency).
pub struct AtomicOperations;

impl AtomicOperations {
    // ---- i32 ----

    /// **NEVER PUBLISHES — operates on a stack-local atomic.**
    ///
    /// See the `AtomicOperations` type-level docs. For a real heap CAS use
    /// `NativeContext::compare_and_swap_value` or build an `&AtomicI32`
    /// directly over the field's slot.
    #[deprecated(
        note = "operates on a stack-local AtomicI32 and never publishes to the \
                heap slot; use `NativeContext::compare_and_swap_value` or an \
                atomic ref into the heap instead. See round-9 CRIT-3."
    )]
    #[cfg(test)]
    pub(crate) fn compare_and_set_int(
        current: i32,
        expected: i32,
        new_value: i32,
        ordering: Ordering,
    ) -> (bool, i32) {
        // The original implementation silently succeeded against a fresh
        // local atomic, then expected the caller to write the value back —
        // a non-atomic read-then-write that loses to any concurrent CAS.
        // We retain the algebra for the in-file unit tests so the deprecation
        // is visible at compile time without breaking the existing test
        // surface; production callers should never reach this code.
        //
        // Round-9 CRIT-3 follow-up: the function is now `#[cfg(test)]` +
        // `pub(crate)` so it cannot be reached from any production build,
        // and the runtime guard is a hard panic (not a debug_assert) for
        // belt-and-braces against future test scaffolding that misuses it.
        assert!(
            cfg!(test),
            "AtomicOperations::compare_and_set_int called outside tests — \
             this helper does not publish; see round-9 CRIT-3."
        );
        let atom = AtomicI32::new(current);
        let result = atom.compare_exchange(expected, new_value, ordering, Ordering::Relaxed);
        match result {
            Ok(prev) => (true, prev),
            Err(prev) => (false, prev),
        }
    }

    /// **NEVER PUBLISHES — operates on a stack-local atomic.** See
    /// [`Self::compare_and_set_int`].
    #[deprecated(
        note = "operates on a stack-local AtomicI32 and never publishes to the \
                heap slot; use `NativeContext::compare_and_swap_value` instead."
    )]
    #[cfg(test)]
    pub(crate) fn compare_and_exchange_int(
        current: i32,
        expected: i32,
        new_value: i32,
        success: Ordering,
        failure: Ordering,
    ) -> i32 {
        assert!(
            cfg!(test),
            "AtomicOperations::compare_and_exchange_int called outside tests — \
             this helper does not publish; see round-9 CRIT-3."
        );
        let atom = AtomicI32::new(current);
        match atom.compare_exchange(expected, new_value, success, failure) {
            Ok(prev) | Err(prev) => prev,
        }
    }

    /// **NEVER PUBLISHES — operates on a stack-local atomic.**
    ///
    /// Atomic get-and-set for `i32`. The RMW runs against a fresh `AtomicI32`
    /// built from a snapshot, so the result is discarded and no heap slot is
    /// updated (lost update). Same defect as the `compare_and_set_*` siblings
    /// (round-9 CRIT-3) — gated to tests so it cannot be reached from
    /// production. The real path is the `Unsafe.getAndSet*` natives
    /// (`native_unsafe_get_and_set_{int,long}`), which RMW the actual field.
    #[deprecated(
        note = "operates on a stack-local AtomicI32 and never publishes to the \
                heap slot; use the `Unsafe.getAndSet*` natives instead. \
                See round-9 CRIT-3."
    )]
    #[cfg(test)]
    pub(crate) fn get_and_set_int(current: i32, new_value: i32) -> i32 {
        assert!(
            cfg!(test),
            "AtomicOperations::get_and_set_int called outside tests — \
             this helper does not publish; see round-9 CRIT-3."
        );
        let atom = AtomicI32::new(current);
        atom.swap(new_value, Ordering::SeqCst)
    }

    /// **NEVER PUBLISHES — operates on a stack-local atomic.** See
    /// [`Self::get_and_set_int`].
    #[deprecated(
        note = "operates on a stack-local AtomicI32 and never publishes to the \
                heap slot; use the `Unsafe.getAndAdd*` natives instead. \
                See round-9 CRIT-3."
    )]
    #[cfg(test)]
    pub(crate) fn get_and_add_int(current: i32, delta: i32) -> i32 {
        assert!(
            cfg!(test),
            "AtomicOperations::get_and_add_int called outside tests — \
             this helper does not publish; see round-9 CRIT-3."
        );
        let atom = AtomicI32::new(current);
        atom.fetch_add(delta, Ordering::SeqCst)
    }

    /// **NEVER PUBLISHES — operates on a stack-local atomic.** See
    /// [`Self::get_and_set_int`].
    #[deprecated(
        note = "operates on a stack-local AtomicI32 and never publishes to the \
                heap slot; use the `Unsafe` bitwise-RMW natives instead. \
                See round-9 CRIT-3."
    )]
    #[cfg(test)]
    pub(crate) fn get_and_or_int(current: i32, mask: i32) -> i32 {
        assert!(
            cfg!(test),
            "AtomicOperations::get_and_or_int called outside tests — \
             this helper does not publish; see round-9 CRIT-3."
        );
        let atom = AtomicI32::new(current);
        atom.fetch_or(mask, Ordering::SeqCst)
    }

    /// **NEVER PUBLISHES — operates on a stack-local atomic.** See
    /// [`Self::get_and_set_int`].
    #[deprecated(
        note = "operates on a stack-local AtomicI32 and never publishes to the \
                heap slot; use the `Unsafe` bitwise-RMW natives instead. \
                See round-9 CRIT-3."
    )]
    #[cfg(test)]
    pub(crate) fn get_and_and_int(current: i32, mask: i32) -> i32 {
        assert!(
            cfg!(test),
            "AtomicOperations::get_and_and_int called outside tests — \
             this helper does not publish; see round-9 CRIT-3."
        );
        let atom = AtomicI32::new(current);
        atom.fetch_and(mask, Ordering::SeqCst)
    }

    /// **NEVER PUBLISHES — operates on a stack-local atomic.** See
    /// [`Self::get_and_set_int`].
    #[deprecated(
        note = "operates on a stack-local AtomicI32 and never publishes to the \
                heap slot; use the `Unsafe` bitwise-RMW natives instead. \
                See round-9 CRIT-3."
    )]
    #[cfg(test)]
    pub(crate) fn get_and_xor_int(current: i32, mask: i32) -> i32 {
        assert!(
            cfg!(test),
            "AtomicOperations::get_and_xor_int called outside tests — \
             this helper does not publish; see round-9 CRIT-3."
        );
        let atom = AtomicI32::new(current);
        atom.fetch_xor(mask, Ordering::SeqCst)
    }

    // ---- i64 ----

    /// **NEVER PUBLISHES — operates on a stack-local atomic.** See
    /// [`Self::compare_and_set_int`].
    #[deprecated(
        note = "operates on a stack-local AtomicI64 and never publishes to the \
                heap slot; use `NativeContext::compare_and_swap_value` instead. \
                See round-9 CRIT-3."
    )]
    #[cfg(test)]
    pub(crate) fn compare_and_set_long(
        current: i64,
        expected: i64,
        new_value: i64,
        ordering: Ordering,
    ) -> (bool, i64) {
        assert!(
            cfg!(test),
            "AtomicOperations::compare_and_set_long called outside tests — \
             this helper does not publish; see round-9 CRIT-3."
        );
        let atom = AtomicI64::new(current);
        let result = atom.compare_exchange(expected, new_value, ordering, Ordering::Relaxed);
        match result {
            Ok(prev) => (true, prev),
            Err(prev) => (false, prev),
        }
    }

    /// **NEVER PUBLISHES — operates on a stack-local atomic.** See
    /// [`Self::compare_and_set_int`].
    #[deprecated(
        note = "operates on a stack-local AtomicI64 and never publishes to the \
                heap slot; use `NativeContext::compare_and_swap_value` instead."
    )]
    #[cfg(test)]
    pub(crate) fn compare_and_exchange_long(
        current: i64,
        expected: i64,
        new_value: i64,
        success: Ordering,
        failure: Ordering,
    ) -> i64 {
        assert!(
            cfg!(test),
            "AtomicOperations::compare_and_exchange_long called outside tests — \
             this helper does not publish; see round-9 CRIT-3."
        );
        let atom = AtomicI64::new(current);
        match atom.compare_exchange(expected, new_value, success, failure) {
            Ok(prev) | Err(prev) => prev,
        }
    }

    /// **NEVER PUBLISHES — operates on a stack-local atomic.** See
    /// [`Self::get_and_set_int`]. The real path is the `Unsafe.getAndSet*`
    /// natives (`native_unsafe_get_and_set_long`).
    #[deprecated(
        note = "operates on a stack-local AtomicI64 and never publishes to the \
                heap slot; use the `Unsafe.getAndSet*` natives instead. \
                See round-9 CRIT-3."
    )]
    #[cfg(test)]
    pub(crate) fn get_and_set_long(current: i64, new_value: i64) -> i64 {
        assert!(
            cfg!(test),
            "AtomicOperations::get_and_set_long called outside tests — \
             this helper does not publish; see round-9 CRIT-3."
        );
        let atom = AtomicI64::new(current);
        atom.swap(new_value, Ordering::SeqCst)
    }

    /// **NEVER PUBLISHES — operates on a stack-local atomic.** See
    /// [`Self::get_and_set_int`].
    #[deprecated(
        note = "operates on a stack-local AtomicI64 and never publishes to the \
                heap slot; use the `Unsafe.getAndAdd*` natives instead. \
                See round-9 CRIT-3."
    )]
    #[cfg(test)]
    pub(crate) fn get_and_add_long(current: i64, delta: i64) -> i64 {
        assert!(
            cfg!(test),
            "AtomicOperations::get_and_add_long called outside tests — \
             this helper does not publish; see round-9 CRIT-3."
        );
        let atom = AtomicI64::new(current);
        atom.fetch_add(delta, Ordering::SeqCst)
    }

    /// **NEVER PUBLISHES — operates on a stack-local atomic.** See
    /// [`Self::get_and_set_int`].
    #[deprecated(
        note = "operates on a stack-local AtomicI64 and never publishes to the \
                heap slot; use the `Unsafe` bitwise-RMW natives instead. \
                See round-9 CRIT-3."
    )]
    #[cfg(test)]
    pub(crate) fn get_and_or_long(current: i64, mask: i64) -> i64 {
        assert!(
            cfg!(test),
            "AtomicOperations::get_and_or_long called outside tests — \
             this helper does not publish; see round-9 CRIT-3."
        );
        let atom = AtomicI64::new(current);
        atom.fetch_or(mask, Ordering::SeqCst)
    }

    /// **NEVER PUBLISHES — operates on a stack-local atomic.** See
    /// [`Self::get_and_set_int`].
    #[deprecated(
        note = "operates on a stack-local AtomicI64 and never publishes to the \
                heap slot; use the `Unsafe` bitwise-RMW natives instead. \
                See round-9 CRIT-3."
    )]
    #[cfg(test)]
    pub(crate) fn get_and_and_long(current: i64, mask: i64) -> i64 {
        assert!(
            cfg!(test),
            "AtomicOperations::get_and_and_long called outside tests — \
             this helper does not publish; see round-9 CRIT-3."
        );
        let atom = AtomicI64::new(current);
        atom.fetch_and(mask, Ordering::SeqCst)
    }

    /// **NEVER PUBLISHES — operates on a stack-local atomic.** See
    /// [`Self::get_and_set_int`].
    #[deprecated(
        note = "operates on a stack-local AtomicI64 and never publishes to the \
                heap slot; use the `Unsafe` bitwise-RMW natives instead. \
                See round-9 CRIT-3."
    )]
    #[cfg(test)]
    pub(crate) fn get_and_xor_long(current: i64, mask: i64) -> i64 {
        assert!(
            cfg!(test),
            "AtomicOperations::get_and_xor_long called outside tests — \
             this helper does not publish; see round-9 CRIT-3."
        );
        let atom = AtomicI64::new(current);
        atom.fetch_xor(mask, Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// VarHandleRegistry
// ---------------------------------------------------------------------------

/// Registry that creates, stores, and looks up `VarHandle` instances by ID.
pub struct VarHandleRegistry {
    handles: HashMap<u64, VarHandle>,
    next_id: u64,
}

impl VarHandleRegistry {
    pub fn new() -> Self {
        Self {
            handles: HashMap::new(),
            next_id: 1,
        }
    }

    /// Allocate the next handle ID.
    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Create a `VarHandle` for an instance field and return its ID.
    pub fn create_instance_field_handle(
        &mut self,
        class_name: &str,
        field_name: &str,
        field_index: usize,
        var_type: VarType,
    ) -> u64 {
        let id = self.alloc_id();
        let handle = VarHandle {
            kind: VarHandleKind::InstanceField {
                class_name: class_name.to_string(),
                field_name: field_name.to_string(),
                field_index,
            },
            supported_modes: VarHandle::supported_modes_for(var_type),
            var_type,
        };
        self.handles.insert(id, handle);
        id
    }

    /// Create a `VarHandle` for a static field and return its ID.
    pub fn create_static_field_handle(
        &mut self,
        class_name: &str,
        field_name: &str,
        field_index: usize,
        var_type: VarType,
    ) -> u64 {
        let id = self.alloc_id();
        let handle = VarHandle {
            kind: VarHandleKind::StaticField {
                class_name: class_name.to_string(),
                field_name: field_name.to_string(),
                field_index,
            },
            supported_modes: VarHandle::supported_modes_for(var_type),
            var_type,
        };
        self.handles.insert(id, handle);
        id
    }

    /// Create a `VarHandle` for array elements and return its ID.
    pub fn create_array_handle(&mut self, element_type: VarType) -> u64 {
        let id = self.alloc_id();
        let handle = VarHandle {
            kind: VarHandleKind::ArrayElement { element_type },
            supported_modes: VarHandle::supported_modes_for(element_type),
            var_type: element_type,
        };
        self.handles.insert(id, handle);
        id
    }

    /// Create a `VarHandle` for ByteBuffer views and return its ID.
    pub fn create_byte_buffer_handle(&mut self, element_type: VarType) -> u64 {
        let id = self.alloc_id();
        let handle = VarHandle {
            kind: VarHandleKind::ByteBufferView { element_type },
            supported_modes: VarHandle::supported_modes_for(element_type),
            var_type: element_type,
        };
        self.handles.insert(id, handle);
        id
    }

    /// Look up a `VarHandle` by ID.
    pub fn get(&self, id: u64) -> Option<&VarHandle> {
        self.handles.get(&id)
    }

    /// Check whether an access mode is supported for the given handle.
    pub fn is_access_mode_supported(&self, id: u64, mode: AccessMode) -> bool {
        self.handles
            .get(&id)
            .map_or(false, |h| h.is_mode_supported(mode))
    }

    /// Return the supported access modes slice for a handle.
    pub fn supported_modes(&self, id: u64) -> Option<&[AccessMode]> {
        self.handles.get(&id).map(|h| h.supported_modes.as_slice())
    }

    /// Total number of registered handles.
    pub fn count(&self) -> usize {
        self.handles.len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(deprecated)] // tests still exercise `AtomicOperations` stack-local
                     // algebra; production callers were never wired up — see
                     // round-9 CRIT-3 on `compare_and_set_int/long`.
mod tests;
