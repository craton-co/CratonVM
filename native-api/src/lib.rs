// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Native method API for CratonVM.
//!
//! Provides the NativeContext trait, NativeMethodRegistry, FFI types,
//! and FileDescriptorTable used by all native method crates.

/// Where a native's PRIVATE slot map may start on a real JDK class, so that no
/// private slot collides with a field the class declares. One implementation;
/// `native-builtins` forwards to it. See W7-68-live-under-allocations.md and
/// W7-49-slot-index-recensus.md §8.
pub mod appended_slots;
/// Can `new` legally produce an instance of this class? The JVMS 6.5
/// predicate behind every fabricated-abstract-receiver fix, shared by the
/// crate that MINTS such receivers and the one that REPORTS their class.
pub mod array_store;
pub mod capability;
pub mod charset;
/// Class-identity answers a native can act on: the ambiguous-vs-absent
/// distinction, and the refusal a by-name lookup is allowed to return.
pub mod class_identity;
/// Failure policy for a Java call a native **delegates** to (`close`, `flush`,
/// …): which throwables the JDK method we stand in for actually catches, and
/// which have to come out. See `delegated_close` for why a blanket
/// `let _ = ctx.invoke_virtual(…)` is not that policy.
pub mod delegated_close;
pub mod fd_table;
pub mod ffi;
/// What a `java.io.File` this VM builds has to contain for the JDK own
/// `File` bytecode to agree with it -- `path` and `prefixLength`, written
/// beside the slot-0 string every producer already writes. Six producers
/// across two crates, which is why the rule lives here rather than in one
/// of them. Sibling of [`path_layout`] and [`appended_slots`].
pub mod file_layout;
pub mod init_level;
pub mod instantiable;
pub mod intrinsic;
/// The layout-alias census — the one detector that sees every native object
/// allocation, not only the fabrication funnel's.
///
/// It lives here, beside the `NativeContext::alloc_object` declaration it
/// observes, because both of its callers (`vm`'s implementation of that method
/// and `native-builtins`' fabrication funnel) depend on this crate and neither
/// depends on the other. See the module header for why that is one detector
/// with two observation points and not two detectors.
pub mod layout_alias;
pub mod native_id;
pub mod native_ring;
/// Receiver classes no supported JDK image declares — the measured table that
/// decides which `Bridge` registrations are `SyntheticStub` by §1.5.
pub mod no_image_receiver;
/// What the HOST says its text encoding is: `native.encoding`,
/// `sun.jnu.encoding` and the three stream encodings, which JEP 400 did NOT
/// pin to UTF-8 (only `file.encoding`).
pub mod os_encoding;
/// The slot map of a synthetic `java/nio/file/Path`, resolved from the
/// platform implementation class instead of hard-coded. Two crates produce
/// that carrier, so its layout is decided in one place.
pub mod path_layout;
pub mod poly_call_site;
/// Where an absorbed failure is **recorded** — `PrintStream`/`PrintWriter`'s
/// `trouble` flag (read back by `checkError()`) and a `Handler`'s
/// `ErrorManager`. Sibling of `delegated_close`: that module decides which
/// throwables a JDK `catch` swallows, this one runs the BODY of the same
/// `catch`. Absorbing without recording is not JDK parity — it is silence.
pub mod print_error_state;
// Registrations retired as contract-1.4 shadows, one measured subsystem at a
// time. Sibling of `no_image_receiver`: both are class/triple-scoped kind
// decisions made centrally because they are MEASUREMENTS against a JDK image
// that no registration site can know.
pub mod plain_server_socket;
/// The READ-side half of the slot-index census: a native reading slot `k` of a
/// real JDK object it did not allocate, where slot `k` on the loaded class
/// means a different field.
///
/// Sibling of `layout_alias`, not an extension of it, and deliberately so:
/// that instrument's whole vocabulary is a slot COUNT, so it cannot say "slot 0
/// is `mark`, not `hb`" — see W7-59-layout-detector-coverage.md section 6 and
/// W7-69-read-side-alias-instrument.md. It reuses `layout_alias`'s flag,
/// because the two are two halves of one species.
pub mod read_alias;
pub mod registry;
pub mod retired_shadow;
pub mod server_socket_ports;
pub mod socket_input_stream_read;
/// The synthetic `java.nio.channels.FileChannel` private slot map. Lives here,
/// not in a native crate, because `native-io` and `native-builtins` both own
/// accessors for it and a map with two owners drifts —
/// W7-72-ssc-socket-and-filechannel.md.
pub mod synthetic_file_channel;
/// Turn named rows of the retirement tables back OFF at runtime, so
/// bisecting a wave costs a run instead of a ~25-minute build. A
/// DIAGNOSTIC: unset -- every shipping configuration -- it is inert, and
/// `the_default_is_inert_across_every_retired_row` asserts that against the
/// whole table rather than a sample. Sibling of [`retired_shadow`], whose
/// tables it reads and never writes.
pub mod unretire;
pub mod vm_scoped;

/// Lightweight `NativeContext` mock available to tests and to other
/// workspace crates that opt in via the `test-mock` feature.
///
/// See `test_mock::MockNativeContext` for the contract — it's the smallest
/// impl that lets trait default methods (`atomic_fetch_add_int`,
/// `set_static_field_by_name`, …) run without standing up a full VM.
#[cfg(any(test, feature = "test-mock"))]
pub mod test_mock;

/// Per-VM capability gate for native and foreign (FFM) operations.
///
/// See `capability` for the model, the three modes, and why the default is
/// permissive. `docs/security/native-capabilities.md` carries the audit
/// inventory and the ordered plan to reach default-deny.
pub use capability::{
    capabilities_for, capability_audit, install_capabilities, uninstall_capabilities, Capability,
    CapabilityAuditReport, CapabilityCheck, CapabilityDenied, CapabilityKind, CapabilityMode,
    CapabilitySet, CapabilityUse, PortSpec, Scope, VmId,
};
pub use class_identity::{refusal_to_java_failure, ClassIdentityError, NameLookup};
pub use delegated_close::{
    absorb_exception, absorb_io_exception, absorb_thrown, vm_only_best_effort,
};
pub use intrinsic::InterpIntrinsic;
/// Native-dispatch call-site memoization: resolve once, then index.
///
/// `NativeMethodRegistry::find` hashes all three of class/method/descriptor on
/// every call. `NativeCallSite` turns the steady-state cost into an atomic load
/// plus an array index; `NativeMethodKey` removes the hash from the sites that
/// still have to resolve by name. See `native_id` for the full rationale.
pub use native_id::{NativeCallSite, NativeMethodId, NativeMethodKey};
pub use print_error_state::{
    absorb_io_exception_recording, absorb_write_exception_recording, classify_write_failure,
    clear_trouble, is_trouble, record_host_io_failure, record_write_failure, report_handler_error,
    set_trouble, take_absorbed, DelegatedWrite,
};
pub use registry::{
    dispatch_baos_event, install_baos_event_hook, AnnotationData, AnnotationElementValue,
    BaosEvent, BaosEventHook, DefineClassFull, FieldMetadata, LambdaSerialMetadata,
    LambdaSerializability, MethodMetadata, NativeCallback, NativeCensusEntry, NativeClassAccess,
    NativeContext, NativeExceptionAccess, NativeGpuAccess, NativeHandle, NativeHandleScope,
    NativeHeapAccess, NativeInvokeAccess, NativeKind, NativeMethodRegistry, NativeSystemAccess,
    NativeThreadAccess, NativeThreadBlocker, StackTraceEntry, ThreadJmxSnapshot,
    TypeArgAnnotations,
};

// ===========================================================================
// Non-allocating class discrimination (perf/collections-classification-cost)
// ===========================================================================

use cratonvm_types::ClassId;

/// A class the native crates repeatedly need to recognise by identity.
///
/// # Why this exists
///
/// [`NativeContext::class_name_of_id`] returns `Option<String>`, and the VM
/// implements it as a `class_manager` **RwLock read acquisition plus a heap
/// `String` allocation**, on every single call. `native-collections` alone has
/// 63 call sites, and its receiver-classification predicates
/// (`is_tree_map_receiver`, `is_chm_receiver`, `is_lhm_receiver`,
/// `is_unmod_wrapper`, `uses_native_hashtable_layout`, …) sit directly on the
/// `HashMap.get`/`put`/`remove` hot path, each walking the superclass chain and
/// calling it once per hop. They compose, so a node-path `HashMap.put` paid on
/// the order of **5-6 lock acquisitions and 5-6 `String` allocations** of pure
/// classification overhead before any lookup work happened — recomputed on
/// every call, for a receiver whose `ClassId` never changes. That is the shape
/// of the measured 21.2x `HashMap` slowdown against JDK 25 C2 (versus 2.2-2.9x
/// on the arithmetic / sieve / matrix rows), and of its sub-linear scaling: a
/// large per-op constant, not a fragmentation signature.
///
/// Resolving the interesting names to `ClassId`s **once** and comparing
/// integers thereafter is strictly better than any string path — no lock, no
/// allocation, no `str` comparison.
///
/// # Identity, not name
///
/// [`ClassDiscriminator`] compares `ClassId`s, so it distinguishes two classes
/// that share a binary name but were defined by different loaders, where a
/// name comparison conflates them. For every member of this enum that
/// distinction is vacuous: `java/lang/Object` and `java/util/*` are
/// bootstrap-only (the `java.*` namespace is loader-protected, so no
/// application loader can define into it), and `cratonvm/internal/*` are
/// VM-synthesised singletons. **Do not extend this enum with an
/// application-loadable class name** without revisiting that argument — for
/// such a name the name-walk is the semantically correct test, not this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum WellKnownClass {
    /// `java/lang/Object` — the terminator of every superclass walk.
    Object,
    /// `java/util/HashMap`.
    HashMap,
    /// `java/util/LinkedHashMap`.
    LinkedHashMap,
    /// `java/util/TreeMap`.
    TreeMap,
    /// `java/util/concurrent/ConcurrentHashMap`.
    ConcurrentHashMap,
    /// `java/util/Hashtable`.
    Hashtable,
    /// `java/util/Properties`.
    Properties,
    /// `java/util/ArrayList`.
    ArrayList,
    /// `cratonvm/internal/UnmodifiableMap`.
    UnmodifiableMap,
    /// `cratonvm/internal/UnmodifiableList`.
    UnmodifiableList,
    /// `cratonvm/internal/UnmodifiableSet`.
    UnmodifiableSet,
    /// `cratonvm/internal/UnmodifiableSortedSet`.
    UnmodifiableSortedSet,
    /// `cratonvm/internal/UnmodifiableNavigableSet`.
    UnmodifiableNavigableSet,
    /// `cratonvm/internal/UnmodifiableEntrySet`.
    UnmodifiableEntrySet,
    /// `cratonvm/internal/UnmodifiableCollection`.
    UnmodifiableCollection,
}

impl WellKnownClass {
    /// Number of variants — the width of every per-VM resolution table.
    pub const COUNT: usize = 15;

    /// Every variant in declaration order, so `ALL[w.index()] == w`.
    pub const ALL: [WellKnownClass; Self::COUNT] = [
        WellKnownClass::Object,
        WellKnownClass::HashMap,
        WellKnownClass::LinkedHashMap,
        WellKnownClass::TreeMap,
        WellKnownClass::ConcurrentHashMap,
        WellKnownClass::Hashtable,
        WellKnownClass::Properties,
        WellKnownClass::ArrayList,
        WellKnownClass::UnmodifiableMap,
        WellKnownClass::UnmodifiableList,
        WellKnownClass::UnmodifiableSet,
        WellKnownClass::UnmodifiableSortedSet,
        WellKnownClass::UnmodifiableNavigableSet,
        WellKnownClass::UnmodifiableEntrySet,
        WellKnownClass::UnmodifiableCollection,
    ];

    /// The JVM binary (slash-separated) name of this class.
    pub const fn binary_name(self) -> &'static str {
        match self {
            WellKnownClass::Object => "java/lang/Object",
            WellKnownClass::HashMap => "java/util/HashMap",
            WellKnownClass::LinkedHashMap => "java/util/LinkedHashMap",
            WellKnownClass::TreeMap => "java/util/TreeMap",
            WellKnownClass::ConcurrentHashMap => "java/util/concurrent/ConcurrentHashMap",
            WellKnownClass::Hashtable => "java/util/Hashtable",
            WellKnownClass::Properties => "java/util/Properties",
            WellKnownClass::ArrayList => "java/util/ArrayList",
            WellKnownClass::UnmodifiableMap => "cratonvm/internal/UnmodifiableMap",
            WellKnownClass::UnmodifiableList => "cratonvm/internal/UnmodifiableList",
            WellKnownClass::UnmodifiableSet => "cratonvm/internal/UnmodifiableSet",
            WellKnownClass::UnmodifiableSortedSet => "cratonvm/internal/UnmodifiableSortedSet",
            WellKnownClass::UnmodifiableNavigableSet => {
                "cratonvm/internal/UnmodifiableNavigableSet"
            }
            WellKnownClass::UnmodifiableEntrySet => "cratonvm/internal/UnmodifiableEntrySet",
            WellKnownClass::UnmodifiableCollection => "cratonvm/internal/UnmodifiableCollection",
        }
    }

    /// Dense index into a `[_; COUNT]` resolution table.
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// Sentinel for "this slot has not resolved yet". A real `ClassId` is a slot
/// index into `ClassStore`'s vector, so `u32::MAX` is unreachable.
const WK_UNRESOLVED: u32 = u32::MAX;

/// Per-VM, per-thread cache of resolved [`WellKnownClass`] `ClassId`s.
struct WellKnownIds {
    /// `None` until first use. Compared against
    /// [`NativeContext::vm_identity`] so a second `Vm` in the same process
    /// (Rust tests create several) can never read the first one's `ClassId`s.
    vm: Option<usize>,
    /// [`WK_UNRESOLVED`] until resolved.
    ///
    /// A **successful** resolution is cached for the lifetime of the VM, with
    /// no generation stamp and no unload hook, because `ClassId`s are never
    /// reissued: `ClassStore::remove` replaces the slot with a `None`
    /// tombstone via `Option::take` and `ClassStore::next_id()` is the slot
    /// vector's `len()`, never `live_count`
    /// (`classloading/src/class.rs:909` and `:1096`; the invariant is pinned
    /// by the test `unloaded_slots_are_tombstoned_and_never_reused`). The only
    /// writers of the slot vector are `push` (append) and `take` (tombstone) —
    /// nothing ever stores `Some(_)` back into an existing slot. So a
    /// `ClassId` that once denoted `java/util/HashMap` denotes it or nothing,
    /// forever; it can never come to denote a different class.
    ///
    /// A **failed** resolution is deliberately not cached: the class may load
    /// later, and a stale negative would silently misclassify every instance
    /// of it.
    ids: [u32; WellKnownClass::COUNT],
}

impl WellKnownIds {
    const fn new() -> Self {
        Self {
            vm: None,
            ids: [WK_UNRESOLVED; WellKnownClass::COUNT],
        }
    }
}

thread_local! {
    static WELL_KNOWN_IDS: std::cell::RefCell<WellKnownIds> =
        const { std::cell::RefCell::new(WellKnownIds::new()) };
}

/// Allocation-free, lock-free-in-steady-state recognition of the handful of
/// classes the native collection code branches on.
///
/// Blanket-implemented for every [`NativeContext`], including
/// `dyn NativeContext`, so no implementor has to do anything. It is an
/// extension trait rather than defaulted methods on `NativeContext` itself
/// purely so that this lands without editing `registry.rs`; the two are
/// equivalent to callers.
///
/// If the `registry.rs` owner would rather have it on the trait, the minimal
/// version to add there is
/// `fn class_name_matches(&self, id: ClassId, name: &str) -> bool` with a
/// default of `self.class_name_of_id(id).is_some_and(|n| n == name)`, which
/// the VM can then override to compare against its interned `Arc<str>` under
/// the read lock without materialising a `String`. That would additionally
/// speed up the *cold* path here; the steady-state path below already needs
/// neither.
pub trait ClassDiscriminator {
    /// The `ClassId` of `which`, or `None` when that class is not loaded.
    ///
    /// Costs one `class_id_by_name` (which does take the `class_manager`
    /// lock) the first time it succeeds on this thread for this VM, and
    /// nothing but a thread-local array read every time after.
    fn well_known_class_id(&self, which: WellKnownClass) -> Option<ClassId>;

    /// `true` iff `class_id` is **exactly** `which` — no superclass walk, no
    /// interface check. This is the direct replacement for
    /// `ctx.class_name_arc_of_id(id).as_deref() == Some("java/util/TreeMap")`.
    fn class_is(&self, class_id: ClassId, which: WellKnownClass) -> bool {
        self.well_known_class_id(which) == Some(class_id)
    }

    /// Which [`WellKnownClass`] `class_id` is, if any.
    ///
    /// **Cold-path API — memoize the answer per `ClassId`.** When some members
    /// are not loaded yet this re-attempts their resolution on every call (it
    /// must: refusing to retry would make an unresolved `TreeMap` slot report
    /// a genuine `TreeMap` receiver as `None`, which is a wrong answer, not a
    /// slow one). Callers on a hot path must cache the result — see
    /// `native-collections`' `receiver_facts`, which calls this once per
    /// `ClassId` per thread and serves every later query from a direct-mapped
    /// cache.
    fn well_known_class(&self, class_id: ClassId) -> Option<WellKnownClass>;
}

impl<C: NativeContext + ?Sized> ClassDiscriminator for C {
    fn well_known_class_id(&self, which: WellKnownClass) -> Option<ClassId> {
        let vm = self.vm_identity();
        let slot = which.index();
        // Scoped borrow: never held across `class_id_by_name`, which re-enters
        // the VM and could in principle come back through this trait.
        let cached = WELL_KNOWN_IDS.with(|cell| {
            let mut table = cell.borrow_mut();
            if table.vm != Some(vm) {
                *table = WellKnownIds::new();
                table.vm = Some(vm);
            }
            table.ids[slot]
        });
        if cached != WK_UNRESOLVED {
            return Some(ClassId::new(cached));
        }
        let resolved = self.class_id_by_name(which.binary_name())?;
        WELL_KNOWN_IDS.with(|cell| {
            let mut table = cell.borrow_mut();
            // Re-check the VM: `class_id_by_name` above may have run against a
            // different context on a reused thread.
            if table.vm == Some(vm) {
                table.ids[slot] = resolved.as_u32();
            }
        });
        Some(resolved)
    }

    fn well_known_class(&self, class_id: ClassId) -> Option<WellKnownClass> {
        let raw = class_id.as_u32();
        if raw == WK_UNRESOLVED {
            // Not a reachable `ClassId`; guarding it keeps the scan below from
            // matching an unresolved sentinel slot.
            return None;
        }
        let vm = self.vm_identity();
        let hit = WELL_KNOWN_IDS.with(|cell| {
            let mut table = cell.borrow_mut();
            if table.vm != Some(vm) {
                *table = WellKnownIds::new();
                table.vm = Some(vm);
            }
            table.ids.iter().position(|&id| id == raw)
        });
        if let Some(slot) = hit {
            return Some(WellKnownClass::ALL[slot]);
        }
        // Miss against what is already resolved. Some members may simply not
        // have loaded yet, so retry them; each call below is a thread-local
        // array read for anything already resolved.
        WellKnownClass::ALL
            .into_iter()
            .find(|&which| self.well_known_class_id(which) == Some(class_id))
    }
}

#[cfg(test)]
mod class_discriminator_tests {
    use super::*;
    use crate::test_mock::MockNativeContext;

    #[test]
    fn binary_names_and_indices_line_up() {
        assert_eq!(WellKnownClass::ALL.len(), WellKnownClass::COUNT);
        for (i, which) in WellKnownClass::ALL.into_iter().enumerate() {
            assert_eq!(which.index(), i, "{which:?} index");
            assert!(!which.binary_name().is_empty());
            assert!(
                !which.binary_name().contains('.'),
                "{which:?} must use the slash-separated binary name"
            );
        }
    }

    #[test]
    fn binary_names_are_distinct() {
        let mut names: Vec<&str> = WellKnownClass::ALL
            .into_iter()
            .map(WellKnownClass::binary_name)
            .collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate binary_name");
    }

    #[test]
    fn unresolved_classes_are_not_misreported() {
        // A `MockNativeContext` loads nothing, so `class_id_by_name` always
        // misses and every query must answer `None` / `false` rather than
        // matching some other slot.
        let ctx = MockNativeContext::default();
        for which in WellKnownClass::ALL {
            assert_eq!(ctx.well_known_class_id(which), None, "{which:?}");
            assert!(!ctx.class_is(ClassId::new(0), which), "{which:?}");
        }
        assert_eq!(ctx.well_known_class(ClassId::new(0)), None);
        // The `u32::MAX` guard: never confuse a real id with the sentinel.
        assert_eq!(ctx.well_known_class(ClassId::new(u32::MAX)), None);
    }
}
