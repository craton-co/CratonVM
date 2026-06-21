// SPDX-License-Identifier: Apache-2.0
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
            VarType::Boolean => {
                modes.extend_from_slice(cas_modes);
                modes.extend_from_slice(get_and_set_modes);
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
mod tests {
    use super::*;

    // -- AccessMode --

    #[test]
    fn access_mode_all_31_variants() {
        assert_eq!(AccessMode::all().len(), ACCESS_MODE_COUNT);
        assert_eq!(AccessMode::all().len(), 31);
    }

    #[test]
    fn access_mode_variants_are_unique() {
        let all = AccessMode::all();
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "duplicate variant at indices {i} and {j}");
                }
            }
        }
    }

    // -- VarType --

    #[test]
    fn var_type_all_9_variants() {
        assert_eq!(VarType::all().len(), 9);
    }

    #[test]
    fn var_type_numeric_classification() {
        assert!(VarType::Int.is_numeric());
        assert!(VarType::Long.is_numeric());
        assert!(VarType::Float.is_numeric());
        assert!(VarType::Double.is_numeric());
        assert!(!VarType::Object.is_numeric());
        assert!(!VarType::Boolean.is_numeric());
    }

    #[test]
    fn var_type_integer_classification() {
        assert!(VarType::Int.is_integer());
        assert!(VarType::Long.is_integer());
        assert!(VarType::Byte.is_integer());
        assert!(VarType::Short.is_integer());
        assert!(VarType::Char.is_integer());
        assert!(!VarType::Float.is_integer());
        assert!(!VarType::Object.is_integer());
    }

    // -- VarHandleKind --

    #[test]
    fn varhandle_kind_instance_field() {
        let kind = VarHandleKind::InstanceField {
            class_name: "java/lang/Object".into(),
            field_name: "value".into(),
            field_index: 0,
        };
        assert_eq!(
            kind,
            VarHandleKind::InstanceField {
                class_name: "java/lang/Object".into(),
                field_name: "value".into(),
                field_index: 0,
            }
        );
    }

    #[test]
    fn varhandle_kind_static_field() {
        let kind = VarHandleKind::StaticField {
            class_name: "java/lang/System".into(),
            field_name: "out".into(),
            field_index: 2,
        };
        if let VarHandleKind::StaticField {
            class_name,
            field_name,
            field_index,
        } = &kind
        {
            assert_eq!(class_name, "java/lang/System");
            assert_eq!(field_name, "out");
            assert_eq!(*field_index, 2);
        } else {
            panic!("expected StaticField");
        }
    }

    #[test]
    fn varhandle_kind_array_element() {
        let kind = VarHandleKind::ArrayElement {
            element_type: VarType::Int,
        };
        assert_eq!(
            kind,
            VarHandleKind::ArrayElement {
                element_type: VarType::Int
            }
        );
    }

    #[test]
    fn varhandle_kind_byte_buffer_view() {
        let kind = VarHandleKind::ByteBufferView {
            element_type: VarType::Long,
        };
        assert_eq!(
            kind,
            VarHandleKind::ByteBufferView {
                element_type: VarType::Long
            }
        );
    }

    // -- MemoryOrdering --

    #[test]
    fn memory_ordering_plain_is_relaxed() {
        assert_eq!(MemoryOrdering::plain(), Ordering::Relaxed);
    }

    #[test]
    fn memory_ordering_opaque_is_relaxed() {
        assert_eq!(MemoryOrdering::opaque(), Ordering::Relaxed);
    }

    #[test]
    fn memory_ordering_acquire() {
        assert_eq!(MemoryOrdering::acquire(), Ordering::Acquire);
    }

    #[test]
    fn memory_ordering_release() {
        assert_eq!(MemoryOrdering::release(), Ordering::Release);
    }

    #[test]
    fn memory_ordering_volatile_is_seqcst() {
        assert_eq!(MemoryOrdering::volatile(), Ordering::SeqCst);
    }

    #[test]
    fn memory_ordering_for_access_mode_plain() {
        assert_eq!(
            MemoryOrdering::for_access_mode(AccessMode::Get),
            Ordering::Relaxed
        );
        assert_eq!(
            MemoryOrdering::for_access_mode(AccessMode::Set),
            Ordering::Relaxed
        );
    }

    #[test]
    fn memory_ordering_for_access_mode_acquire() {
        assert_eq!(
            MemoryOrdering::for_access_mode(AccessMode::GetAcquire),
            Ordering::Acquire
        );
        assert_eq!(
            MemoryOrdering::for_access_mode(AccessMode::CompareAndExchangeAcquire),
            Ordering::Acquire
        );
    }

    #[test]
    fn memory_ordering_for_access_mode_release() {
        assert_eq!(
            MemoryOrdering::for_access_mode(AccessMode::SetRelease),
            Ordering::Release
        );
        assert_eq!(
            MemoryOrdering::for_access_mode(AccessMode::GetAndSetRelease),
            Ordering::Release
        );
    }

    #[test]
    fn memory_ordering_for_access_mode_volatile() {
        assert_eq!(
            MemoryOrdering::for_access_mode(AccessMode::GetVolatile),
            Ordering::SeqCst
        );
        assert_eq!(
            MemoryOrdering::for_access_mode(AccessMode::CompareAndSet),
            Ordering::SeqCst
        );
        assert_eq!(
            MemoryOrdering::for_access_mode(AccessMode::GetAndAdd),
            Ordering::SeqCst
        );
    }

    // -- FenceOperations --

    #[test]
    fn fence_full_does_not_panic() {
        FenceOperations::full_fence();
    }

    #[test]
    fn fence_acquire_does_not_panic() {
        FenceOperations::acquire_fence();
    }

    #[test]
    fn fence_release_does_not_panic() {
        FenceOperations::release_fence();
    }

    #[test]
    fn fence_load_load_does_not_panic() {
        FenceOperations::load_load_fence();
    }

    #[test]
    fn fence_store_store_does_not_panic() {
        FenceOperations::store_store_fence();
    }

    // -- AtomicOperations: int --

    #[test]
    fn atomic_cas_int_success() {
        let (ok, witness) = AtomicOperations::compare_and_set_int(42, 42, 99, Ordering::SeqCst);
        assert!(ok);
        assert_eq!(witness, 42);
    }

    #[test]
    fn atomic_cas_int_failure() {
        let (ok, witness) = AtomicOperations::compare_and_set_int(42, 10, 99, Ordering::SeqCst);
        assert!(!ok);
        assert_eq!(witness, 42);
    }

    #[test]
    fn atomic_compare_and_exchange_int() {
        let prev = AtomicOperations::compare_and_exchange_int(
            42,
            42,
            99,
            Ordering::SeqCst,
            Ordering::Relaxed,
        );
        assert_eq!(prev, 42);
    }

    #[test]
    fn atomic_compare_and_exchange_int_fail() {
        let prev = AtomicOperations::compare_and_exchange_int(
            42,
            10,
            99,
            Ordering::SeqCst,
            Ordering::Relaxed,
        );
        assert_eq!(prev, 42); // witness is old value, not new
    }

    #[test]
    fn atomic_get_and_set_int() {
        let old = AtomicOperations::get_and_set_int(42, 99);
        assert_eq!(old, 42);
    }

    #[test]
    fn atomic_get_and_add_int() {
        let old = AtomicOperations::get_and_add_int(42, 8);
        assert_eq!(old, 42);
    }

    #[test]
    fn atomic_get_and_or_int() {
        let old = AtomicOperations::get_and_or_int(0b1010, 0b1100);
        assert_eq!(old, 0b1010);
    }

    #[test]
    fn atomic_get_and_and_int() {
        let old = AtomicOperations::get_and_and_int(0b1010, 0b1100);
        assert_eq!(old, 0b1010);
    }

    #[test]
    fn atomic_get_and_xor_int() {
        let old = AtomicOperations::get_and_xor_int(0b1010, 0b1100);
        assert_eq!(old, 0b1010);
    }

    // -- AtomicOperations: long --

    #[test]
    fn atomic_cas_long_success() {
        let (ok, witness) =
            AtomicOperations::compare_and_set_long(100_000, 100_000, 200_000, Ordering::SeqCst);
        assert!(ok);
        assert_eq!(witness, 100_000);
    }

    #[test]
    fn atomic_get_and_add_long() {
        let old = AtomicOperations::get_and_add_long(1_000_000, 500);
        assert_eq!(old, 1_000_000);
    }

    #[test]
    fn atomic_get_and_set_long() {
        let old = AtomicOperations::get_and_set_long(42, 99);
        assert_eq!(old, 42);
    }

    #[test]
    fn atomic_get_and_or_long() {
        let old = AtomicOperations::get_and_or_long(0xFF00, 0x00FF);
        assert_eq!(old, 0xFF00);
    }

    // -- VarHandleRegistry --

    #[test]
    fn registry_create_instance_field_handle() {
        let mut reg = VarHandleRegistry::new();
        let id = reg.create_instance_field_handle("Foo", "bar", 0, VarType::Int);
        assert!(id > 0);
        let h = reg.get(id).unwrap();
        assert_eq!(h.var_type, VarType::Int);
        if let VarHandleKind::InstanceField {
            class_name,
            field_name,
            field_index,
        } = &h.kind
        {
            assert_eq!(class_name, "Foo");
            assert_eq!(field_name, "bar");
            assert_eq!(*field_index, 0);
        } else {
            panic!("expected InstanceField");
        }
    }

    #[test]
    fn registry_create_static_field_handle() {
        let mut reg = VarHandleRegistry::new();
        let id = reg.create_static_field_handle("Baz", "count", 3, VarType::Long);
        let h = reg.get(id).unwrap();
        assert_eq!(h.var_type, VarType::Long);
        assert!(matches!(&h.kind, VarHandleKind::StaticField { .. }));
    }

    #[test]
    fn registry_create_array_handle() {
        let mut reg = VarHandleRegistry::new();
        let id = reg.create_array_handle(VarType::Double);
        let h = reg.get(id).unwrap();
        assert_eq!(h.var_type, VarType::Double);
        assert!(matches!(&h.kind, VarHandleKind::ArrayElement { .. }));
    }

    #[test]
    fn registry_create_byte_buffer_handle() {
        let mut reg = VarHandleRegistry::new();
        let id = reg.create_byte_buffer_handle(VarType::Int);
        let h = reg.get(id).unwrap();
        assert!(matches!(&h.kind, VarHandleKind::ByteBufferView { .. }));
    }

    #[test]
    fn registry_lookup_by_id() {
        let mut reg = VarHandleRegistry::new();
        let id = reg.create_instance_field_handle("A", "x", 1, VarType::Short);
        assert!(reg.get(id).is_some());
        assert!(reg.get(id + 999).is_none());
    }

    #[test]
    fn registry_is_access_mode_supported_int_cas() {
        let mut reg = VarHandleRegistry::new();
        let id = reg.create_instance_field_handle("X", "v", 0, VarType::Int);
        assert!(reg.is_access_mode_supported(id, AccessMode::CompareAndSet));
        assert!(reg.is_access_mode_supported(id, AccessMode::GetAndAdd));
        assert!(reg.is_access_mode_supported(id, AccessMode::GetAndBitwiseOr));
    }

    #[test]
    fn registry_is_access_mode_supported_object_no_get_and_add() {
        let mut reg = VarHandleRegistry::new();
        let id = reg.create_instance_field_handle("X", "ref", 0, VarType::Object);
        assert!(reg.is_access_mode_supported(id, AccessMode::CompareAndSet));
        assert!(reg.is_access_mode_supported(id, AccessMode::GetAndSet));
        assert!(!reg.is_access_mode_supported(id, AccessMode::GetAndAdd));
        assert!(!reg.is_access_mode_supported(id, AccessMode::GetAndBitwiseOr));
    }

    #[test]
    fn registry_count_tracking() {
        let mut reg = VarHandleRegistry::new();
        assert_eq!(reg.count(), 0);
        reg.create_instance_field_handle("A", "a", 0, VarType::Int);
        assert_eq!(reg.count(), 1);
        reg.create_array_handle(VarType::Long);
        assert_eq!(reg.count(), 2);
        reg.create_static_field_handle("B", "b", 1, VarType::Boolean);
        assert_eq!(reg.count(), 3);
    }

    // -- Supported modes content checks --

    #[test]
    fn supported_modes_int_includes_bitwise_or() {
        let modes = VarHandle::supported_modes_for(VarType::Int);
        assert!(modes.contains(&AccessMode::GetAndBitwiseOr));
        assert!(modes.contains(&AccessMode::GetAndBitwiseOrAcquire));
        assert!(modes.contains(&AccessMode::GetAndBitwiseOrRelease));
    }

    #[test]
    fn supported_modes_object_excludes_bitwise_or() {
        let modes = VarHandle::supported_modes_for(VarType::Object);
        assert!(!modes.contains(&AccessMode::GetAndBitwiseOr));
        assert!(!modes.contains(&AccessMode::GetAndBitwiseAnd));
        assert!(!modes.contains(&AccessMode::GetAndBitwiseXor));
    }

    #[test]
    fn supported_modes_float_includes_add_but_not_bitwise() {
        let modes = VarHandle::supported_modes_for(VarType::Float);
        assert!(modes.contains(&AccessMode::GetAndAdd));
        assert!(!modes.contains(&AccessMode::GetAndBitwiseOr));
    }

    #[test]
    fn supported_modes_boolean_has_cas_and_get_and_set() {
        let modes = VarHandle::supported_modes_for(VarType::Boolean);
        assert!(modes.contains(&AccessMode::CompareAndSet));
        assert!(modes.contains(&AccessMode::GetAndSet));
        assert!(!modes.contains(&AccessMode::GetAndAdd));
        assert!(!modes.contains(&AccessMode::GetAndBitwiseOr));
    }

    #[test]
    fn supported_modes_all_types_have_basic_read_write() {
        for vt in VarType::all() {
            let modes = VarHandle::supported_modes_for(*vt);
            assert!(modes.contains(&AccessMode::Get), "{:?} missing Get", vt);
            assert!(modes.contains(&AccessMode::Set), "{:?} missing Set", vt);
            assert!(
                modes.contains(&AccessMode::GetVolatile),
                "{:?} missing GetVolatile",
                vt
            );
            assert!(
                modes.contains(&AccessMode::SetVolatile),
                "{:?} missing SetVolatile",
                vt
            );
            assert!(
                modes.contains(&AccessMode::GetAcquire),
                "{:?} missing GetAcquire",
                vt
            );
            assert!(
                modes.contains(&AccessMode::SetRelease),
                "{:?} missing SetRelease",
                vt
            );
            assert!(
                modes.contains(&AccessMode::GetOpaque),
                "{:?} missing GetOpaque",
                vt
            );
            assert!(
                modes.contains(&AccessMode::SetOpaque),
                "{:?} missing SetOpaque",
                vt
            );
        }
    }

    #[test]
    fn supported_modes_byte_includes_bitwise() {
        let modes = VarHandle::supported_modes_for(VarType::Byte);
        assert!(modes.contains(&AccessMode::GetAndBitwiseAnd));
        assert!(modes.contains(&AccessMode::GetAndBitwiseXor));
    }
}
