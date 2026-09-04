// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Shared types for the CratonVM project.
//!
//! This crate contains foundational types used by all subsystems:
//! value representation, class identifiers, heap layout constants,
//! and error types.

pub mod access_flags;
pub mod arraylist_view;
mod class_id;
pub mod compact_value;
pub mod compat;
pub mod error;
pub mod fdlibm;
pub mod field_layout;

pub mod ffm_epoch;
pub mod field_watch;
pub mod flag_groups;
pub mod flags;
pub mod float_format;
pub mod handle;
mod heap_types;
pub mod identity_side_tables;
pub mod intern;
pub mod jfp;
pub mod jit_activation;
pub mod loader_pin;
pub mod lock_order;
pub mod metadata_pin;
pub mod mirror_pin;
pub mod narrow_oop;
pub mod reflective_probe;
pub mod striped_counter;
pub mod subsystem_config;
mod value;

/// One collection's `old address -> new address` relocation table.
///
/// Hashed with `FxHasher`, not the default `SipHash`. The keys are heap
/// addresses produced by the collector itself, so there is no adversarial-input
/// concern, and the table is both built and consumed once per collection at a
/// size proportional to the live set — on a Spring workload that is ~756 000
/// entries per young collection. The moving collector already accumulated into
/// an `FxHashMap` for exactly that reason and then paid to convert it into a
/// `HashMap` for this field: 43 ms of a 424 ms stop-the-world pause, to change
/// a container type. Naming the hasher here is what removes that conversion.
/// Since 2026-09-02 (gen-gc-five) this is a SHARDED map built in parallel
/// by the evacuation workers rather than the flat `FxHashMap` alias; the
/// reasoning above still holds for every shard. See [`pointer_map`].
pub mod pointer_map;
pub use pointer_map::PointerMap;

pub use class_id::{ClassId, ClassLoaderId};
pub use compact_value::{CompactTag, CompactValue, CompactValueError};
// The JDK-only policy token. `types` is the only crate that `native-api`,
// `classloading`, `vm` and `vm-cli` all already depend on, so the shared
// `CompatibilityMode` / `ExecutionPolicy` pair lives here (design contract
// `docs/feature-designs/jdk-only-mode.md` §2). Re-exported at the crate root
// as well as under `compat::` because `vm/src/config.rs` re-exports it onward
// as `pub use cratonvm_types::compat::{CompatibilityMode, ExecutionPolicy};`
// and several call sites name it as `cratonvm_types::CompatibilityMode`.
pub use compat::{CompatibilityMode, ExecutionPolicy};
#[cfg(any(test, debug_assertions))]
pub use field_layout::clear_class_layouts;
pub use field_layout::{
    class_layout, class_layout_for_fields, compact_field_slot, compact_field_storage,
    compact_object_body_size, compact_object_field_storage, compact_ref_fields_enabled,
    compact_tlab_body_size, single_layout_domain,
    foreign_layout_refusals, is_compact_object, layout_generation, layout_replace_guard,
    next_layout_domain, object_body_size, pack_fields_by_width_enabled, read_compact_field,
    register_class_layout, set_compact_ref_fields_enabled, set_pack_fields_by_width_enabled,
    unregister_class_layout, with_class_layout, write_compact_field, CompactLayout,
    FieldStorageKind, FIRST_LAYOUT_DOMAIN,
};
pub use flags::{
    flags, install as install_flags, BlockedAccessMode, EnvSource, FlagSource, GcFlags, IoFlags,
    JitFlags, LoaderFlags, MapSource, OverlaySource, VmFlags,
};
// The typed configuration for the subsystems migrated off direct `std::env`
// reads (report P1). Re-exported at the crate root alongside `flags` because
// the call sites that must move here — `jit`, `gc`, `vm`, `native-api` — name
// the flag surface as `cratonvm_types::…` today and should not have to learn a
// second path to reach the same one snapshot.
pub use float_format::{java_double_to_string, java_float_to_string};
pub use handle::{HandleScope, HandleStorage, RootedHandle};
pub use subsystem_config::{
    capability as capability_config, gc_metrics as gc_metrics_config,
    jit_metrics as jit_metrics_config, jit_verify as jit_verify_config, subsystems,
    thread_stress as thread_stress_config, CapabilityConfig, GcMetricsConfig, JitMetricsConfig,
    JitVerifyConfig, SubsystemConfig, ThreadStressConfig,
};
// `mod heap_types` is private, so this list is the *only* way anything outside
// this crate can name a heap constant. A `pub const` added to `heap_types.rs`
// and left off this list is not merely inconvenient — it is unreachable from
// every other crate, i.e. an accidental default-off landing. `MARK_FORWARDED`,
// `FORWARDING_PTR_MASK` and `IDENTITY_HASH_CODE_OFFSET` were in exactly that
// state until 2026-07-26; see `arch-2026-07-26/header-shrink.md` §6.2 and the
// `every_public_heap_constant_is_reachable` test below.
pub use heap_types::{
    array_data_size, array_data_size_checked, array_element_type_from_tag, element_byte_size,
    element_type_tag_at, kind_tag_at, object_kind_from_tag, oob_index_code,
    plausible_object_header_at,
    primitive_array_kind_tags_byte, ArrayElementType, ObjectHeader, ObjectKind,
    ARRAY_DATA_OFFSET, ARRAY_LENGTH_OFFSET, ARRAY_STORE_OUT_OF_MEMORY, AUTOBOX_CLASS_ID,
    FIELD_CELL_PAYLOAD32_OFFSET, FIELD_CELL_PAYLOAD64_OFFSET,
    FIELD_CELL_TAG_OBJECT, FIELD_CELL_TAG_OFFSET, FORWARDING_PTR_MASK, GC_FLAGS_BYTE_OFFSET,
    GC_FLAG_COMPACT, GC_FLAG_MARKED, GC_FLAG_OLD_GEN, HEADER_SIZE, INFLATED_PTR_MASK,
    KIND_TAGS_BYTE_OFFSET, KIND_TAG_BYTE_MASK, MARK_FORWARDED, MARK_HASH_MASK, MARK_HASH_SHIFT,
    MARK_INFLATED, MARK_NEUTRAL, MARK_QUARTET_MASK, MARK_QUARTET_SHIFT, MARK_STATE_MASK,
    MARK_THIN_LOCKED, MARK_WORD_OFFSET, MAX_GC_AGE, NUM_SLOTS_OFFSET, REF_ELEMENT_SIZE,
    REF_FIELD_SIZE, SLOT_SIZE, THIN_LOCK_OWNER_MASK, THIN_LOCK_OWNER_SHIFT,
    THIN_LOCK_RECURSION_MASK, THIN_LOCK_RECURSION_SHIFT,
};
pub use intern::{intern, intern_arc, StringPool};
pub use narrow_oop::{
    narrow_oops_enabled, ref_element_size, ref_field_size, NARROW_REF_SIZE, WIDE_REF_SIZE,
};
pub use value::{
    decode_value, decode_value_checked, encode_value, is_object_tag,
    jlong_bits_as_aligned_object_ptr, plausible_heap_pointer, read_value_atomic,
    read_value_checked, read_value_checked_atomic, write_value_atomic, ObjectRef, RawSlot,
    SlotType, Value, VALUE_MAX_DISCRIMINANT, VTAG_DOUBLE, VTAG_FLOAT, VTAG_INT, VTAG_LONG,
    VTAG_NULL, VTAG_OBJECT, VTAG_RETADDR, VTAG_UNINIT,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that all public re-exports from lib.rs are accessible and usable.
    #[test]
    fn reexport_class_id() {
        let id = ClassId::new(42);
        assert_eq!(id.as_u32(), 42);
    }

    #[test]
    fn reexport_class_loader_id() {
        let _ = ClassLoaderId::Bootstrap;
        let _ = ClassLoaderId::Extension;
        let _ = ClassLoaderId::Application;
    }

    /// `compat.rs` is only reachable because of the `pub mod compat;` above.
    /// Without it the module is orphaned: `native-api`, `classloading`, `vm`
    /// and `vm-cli` all name `cratonvm_types::compat::CompatibilityMode`, and
    /// every one of them fails to compile. The root re-export is the second
    /// half — `vm/src/config.rs` re-exports it onward, so dropping it is the
    /// same class of silent breakage the heap-constant test below guards.
    #[test]
    fn reexport_compat_policy() {
        assert_eq!(CompatibilityMode::default(), CompatibilityMode::Compatible);
        assert!(CompatibilityMode::JdkOnly.is_jdk_only());
        assert_eq!(CompatibilityMode::JdkOnly.as_str(), "jdk-only");

        let policy = ExecutionPolicy::jdk_only();
        assert!(policy.is_jdk_only());
        assert!(policy.real_jdk);
        assert!(!ExecutionPolicy::default().is_jdk_only());

        // The two paths to the same type must be the same type, not a copy.
        let via_module: compat::CompatibilityMode = CompatibilityMode::JdkOnly;
        assert_eq!(via_module, CompatibilityMode::JdkOnly);
    }

    #[test]
    fn reexport_value_types() {
        let v = Value::Int(1);
        assert_eq!(v.as_int(), Some(1));

        let (bits, tag) = encode_value(Value::Long(99));
        assert_eq!(decode_value(bits, tag).as_long(), Some(99));
    }

    #[test]
    fn reexport_value_tags() {
        assert_eq!(VTAG_INT, 0);
        assert_eq!(VTAG_LONG, 1);
        assert_eq!(VTAG_FLOAT, 2);
        assert_eq!(VTAG_DOUBLE, 3);
        assert_eq!(VTAG_OBJECT, 4);
        assert_eq!(VTAG_NULL, 5);
        assert_eq!(VTAG_UNINIT, 6);
        assert_eq!(VTAG_RETADDR, 7);
    }

    #[test]
    fn reexport_is_object_tag() {
        assert!(is_object_tag(VTAG_OBJECT));
        assert!(!is_object_tag(VTAG_NULL));
        assert!(!is_object_tag(VTAG_INT));
    }

    /// The re-exported heap constants, checked against the properties that the
    /// rest of the workspace actually depends on.
    ///
    /// This used to open with `assert_eq!(HEADER_SIZE, 32);`. That pinned one
    /// historical *value* rather than any invariant: it records what the header
    /// happened to be, so a deliberate, fully-correct shrink trips it and a
    /// careless change that keeps the size but breaks alignment does not. It
    /// also cannot say *why* 32 mattered, which is the only thing the person
    /// reading the failure needs.
    ///
    /// Replaced — not deleted — with the three properties that are load-bearing
    /// and that a layout change can actually violate:
    ///
    /// 1. `HEADER_SIZE` stays on the 8-byte grid. Every heap walk, the inline
    ///    TLAB cursor bump and the qword body-zeroing loop step from it, and
    ///    the 8-byte `mark_word` must stay naturally aligned for the CAS on the
    ///    lock fast path.
    /// 2. `HEADER_SIZE <= 127`. The JIT emits array element addressing as
    ///    `[base + index*scale + HEADER_SIZE]` with a **signed** `disp8`. Past
    ///    127 the byte is read back as negative and the emitted load addresses
    ///    memory *before* the object — no panic, just a wrong address. Same for
    ///    `ARRAY_LENGTH_OFFSET`, which is the `disp8` of the bounds-check load.
    /// 3. The header is big enough to hold the fields whose offsets are also
    ///    exported, so no exported offset can point outside it.
    ///
    /// `heap_types.rs` pins (2) at compile time; keeping it here as well means
    /// the constraint is stated where the constant is *published*, and the
    /// failure message names the emitter that breaks.
    #[test]
    fn reexport_heap_constants() {
        assert_eq!(
            HEADER_SIZE % 8,
            0,
            "HEADER_SIZE anchors the 8-byte object grid: heap walks, the inline \
             TLAB cursor bump and the qword body-zeroing loop all step from it"
        );
        assert!(
            HEADER_SIZE >= MARK_WORD_OFFSET + 8,
            "the 8-byte mark word must fit inside the header"
        );
        assert!(
            HEADER_SIZE <= 127,
            "HEADER_SIZE is emitted as a signed disp8 in the JIT's array element \
             addressing (jit/src/x64.rs and jit/src/ir_lower.rs); above 127 the \
             displacement byte reads as negative and the load addresses memory \
             before the object"
        );
        assert!(
            ARRAY_LENGTH_OFFSET <= 127,
            "ARRAY_LENGTH_OFFSET is the signed disp8 of the JIT's array-length \
             load; same failure mode as HEADER_SIZE above"
        );
        assert!(ARRAY_LENGTH_OFFSET > 0);
        // The length word bounds against the *data* offset, not the header
        // size. They are the same today; they stop being the same at
        // HEADER_SIZE = 16, where the length moves into an 8-byte prefix at the
        // head of the array's body and only `ARRAY_DATA_OFFSET` still sits past
        // it. Stating it against `HEADER_SIZE` would make this assert fail on a
        // correct layout, which is the wrong way for an invariant to break.
        assert!(ARRAY_LENGTH_OFFSET + 4 <= ARRAY_DATA_OFFSET);
        assert!(
            ARRAY_DATA_OFFSET >= HEADER_SIZE && ARRAY_DATA_OFFSET % 8 == 0,
            "array data starts at or past the header end, on the 8-byte grid"
        );
        assert!(
            ARRAY_DATA_OFFSET <= 127,
            "ARRAY_DATA_OFFSET is the signed disp8 of the JIT's array element \
             addressing; same failure mode as HEADER_SIZE above"
        );
        assert_eq!(SLOT_SIZE, 16);
        assert_eq!(REF_ELEMENT_SIZE, 8);
        assert_eq!(AUTOBOX_CLASS_ID.as_u32(), u32::MAX);
    }

    /// `mod heap_types` is private. Anything it declares `pub` but that is left
    /// off the `pub use heap_types::{…}` list above is unreachable from every
    /// other crate in the workspace — the code exists and nothing can call it,
    /// which is a default-off landing arrived at by omission rather than by
    /// choice.
    ///
    /// That is not hypothetical: `MARK_FORWARDED`, `FORWARDING_PTR_MASK` and
    /// `IDENTITY_HASH_CODE_OFFSET` all landed in `heap_types.rs` on 2026-07-26
    /// and were stranded exactly this way. Naming them here means the
    /// re-export cannot be dropped again without failing to compile.
    #[test]
    fn every_public_heap_constant_is_reachable() {
        // The mark-word encoding for a relocated object. Consumers that match
        // on `ObjectHeader::mark_state(m)` need the tag constant itself, not
        // just the `is_forwarded_mark` helper.
        assert_eq!(MARK_FORWARDED & MARK_STATE_MASK, MARK_FORWARDED);
        assert_ne!(MARK_FORWARDED, MARK_NEUTRAL);
        assert_ne!(MARK_FORWARDED, MARK_THIN_LOCKED);
        assert_ne!(MARK_FORWARDED, MARK_INFLATED);

        // The forwarding pointer occupies every bit the state tag does not, so
        // an aligned address round-trips through the mark word exactly.
        assert_eq!(FORWARDING_PTR_MASK, !MARK_STATE_MASK);
        assert_eq!(FORWARDING_PTR_MASK & MARK_STATE_MASK, 0);
        let target = 0x0000_7fff_dead_b000u64;
        assert_eq!(target & MARK_STATE_MASK, 0, "test address must be aligned");
        assert_eq!((target | MARK_FORWARDED) & FORWARDING_PTR_MASK, target);

        // The last header field that had no named constant. `jit/src/x64.rs`
        // derived its own via `offset_of!` and `vm/src/jit/helpers.rs` still
        // writes a bare `raw_ptr.add(8)`; both should use this.

        // The JIT's inline `checkcast` fast path bakes this byte as an
        // immediate, so the packing it assumes is asserted here rather than
        // trusted: `kind` in bits 0..1, `element_type` in bits 2..5, bits 6..7
        // reserved zero. A `byte[]` is 0x21 and nothing else is.
        assert_eq!(primitive_array_kind_tags_byte("[B"), Some(0x21));
        assert_eq!(
            primitive_array_kind_tags_byte("[I"),
            Some(ObjectKind::Array as u8 | ((ArrayElementType::Int as u8) << 2)),
        );
        for d in ["[Z", "[C", "[F", "[D", "[B", "[S", "[I", "[J"] {
            let tag = primitive_array_kind_tags_byte(d).expect(d);
            assert_eq!(tag & KIND_TAG_BYTE_MASK, ObjectKind::Array as u8);
            assert_eq!(tag & 0xC0, 0, "bits 6..7 are reserved zero: {d}");
        }
        // ONE dimension only. `[[B` holds references to `byte[]` objects, so
        // its element type is `Reference` and this predicate must not claim it;
        // reference arrays and plain classes have no answer here either.
        for d in [
            "[[B",
            "[Ljava/lang/String;",
            "java/lang/String",
            "[",
            "",
            "B",
        ] {
            assert_eq!(primitive_array_kind_tags_byte(d), None, "{d}");
        }
    }

    #[test]
    fn reexport_heap_types() {
        let _ = ObjectKind::Object;
        let _ = ObjectKind::Array;
        let _ = ArrayElementType::Int;

        assert_eq!(element_byte_size(ArrayElementType::Int), 4);
        assert_eq!(array_data_size(10, ArrayElementType::Int).unwrap(), 40);
        assert_eq!(array_data_size_checked(10, ArrayElementType::Int), Some(40));
        assert_eq!(object_kind_from_tag(1), Some(ObjectKind::Array));
        assert_eq!(object_kind_from_tag(0x7f), None);
        assert_eq!(array_element_type_from_tag(10), Some(ArrayElementType::Int));
        assert_eq!(array_element_type_from_tag(0x7f), None);
    }

    #[test]
    fn reexport_gc_flags() {
        assert_eq!(GC_FLAG_OLD_GEN, 0x01);
        assert_eq!(GC_FLAG_MARKED, 0x02);
    }

    #[test]
    fn reexport_object_header() {
        let header = ObjectHeader::new(
            ClassId::new(0),
            ObjectKind::Object,
            ArrayElementType::Boolean,
            0,
            0,
        );
        assert!(!header.is_forwarded());
        assert!(!header.is_old_gen());
    }

    #[test]
    fn reexport_object_ref() {
        let fake_ptr = 0x1000_u64 as *mut u8;
        let r = unsafe { ObjectRef::from_raw(fake_ptr) };
        assert_eq!(r.as_ptr() as u64, 0x1000);
    }

    #[test]
    fn reexport_handle_types() {
        struct S {
            slots: Vec<Option<ObjectRef>>,
        }
        impl HandleStorage for S {
            fn root(&mut self, r: ObjectRef) -> u32 {
                self.slots.push(Some(r));
                (self.slots.len() - 1) as u32
            }
            fn unroot(&mut self, slot: u32) {
                self.slots[slot as usize] = None;
            }
            fn get(&self, slot: u32) -> ObjectRef {
                self.slots[slot as usize].unwrap()
            }
        }
        let mut storage = S { slots: Vec::new() };
        let fake_ptr = 0x2000_u64 as *mut u8;
        let r = unsafe { ObjectRef::from_raw(fake_ptr) };
        let handle = RootedHandle::new(&mut storage, r);
        assert_eq!(handle.get(&storage), r);
        let mut scope = HandleScope::new(&mut storage);
        let h2 = scope.root(r);
        assert_eq!(scope.get(&h2), r);
    }
}

// ---------------------------------------------------------------------------
// ZGC read-barrier codegen gate
// ---------------------------------------------------------------------------

/// Whether **any** ZGC heap in this process has armed its read-path load
/// barrier -- the codegen gate for stage (a) of `zgc-jit-load-barrier.md`.
///
/// # Why it lives HERE, and why it is process-wide
///
/// Two constraints meet. `gc/src/zgc_concurrent.rs` states the rule this
/// breaks -- "No process-global state ... this tree has had parallel-test
/// crashes from process-global GC caches" -- so the exception needs a reason.
/// And `cratonvm-jit` depends on `cratonvm-gc` only as a **dev-dependency**,
/// deliberately (see the acyclicity note in `jit/Cargo.toml`), so the gate
/// cannot live in the gc crate without adding a real edge.
///
/// `cratonvm-types` is the crate both already depend on and which depends on
/// nothing, so it is where a fact shared by the collector and the code
/// generator belongs.
///
/// The reason it must be process-wide at all: the **consumer** is the JIT's
/// code generator, deciding whether to emit an inline reference load while
/// holding no heap handle and, for shared code, on behalf of no particular VM.
///
/// The direction of the error makes it safe. The only thing this can get wrong
/// in a multi-VM process is make a second VM's JIT route reference loads
/// through the helpers when its own heap has no barrier armed -- a throughput
/// loss. The reverse, a heap arming its barrier while some other VM's JIT goes
/// on emitting raw inline loads, is the use-after-free, and it cannot happen,
/// because arming sets the flag for everyone.
#[inline]
pub fn zgc_read_barrier_armed() -> bool {
    ZGC_READ_BARRIER_ARMED.load(std::sync::atomic::Ordering::Acquire)
}

/// Publish the read-barrier arming state. Called by
/// `ZgcRealHeap::set_barrier_color`; see [`zgc_read_barrier_armed`].
#[inline]
pub fn set_zgc_read_barrier_armed(armed: bool) {
    ZGC_READ_BARRIER_ARMED.store(armed, std::sync::atomic::Ordering::Release);
}

static ZGC_READ_BARRIER_ARMED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Whether the collector must be TOLD about each object a thread
/// bump-allocates out of its TLAB, rather than discovering it by walking the
/// chunk.
///
/// The two linear-sweep collectors (Generational, G1) parse a TLAB chunk as
/// memory, so an object that merely appears in one needs no announcement.
/// ZGC's sweep, its `is_object_address` oracle and its conservative scans are
/// driven by an allocation-base REGISTRY instead, so an object it was never
/// told about does not exist as far as the runtime is concerned — a receiver
/// allocated that way decodes as `null` at the next native boundary.
///
/// The JIT's inline allocator normally SKIPS its post-allocation helper when
/// the class needs no primitive initialisation and has no finalizer
/// (`skip_post_init_helper` in `x64::objects::emit_inline_tlab_new`), because
/// on those backends the helper would have nothing left to do. That helper is
/// also the only place an inline-allocated object can be announced, so this
/// flag forces the call back on. Published by `ZgcRealHeap` when it hands VM
/// TLABs out; read at JIT compile time, so it must be set before the first
/// compile — heap construction is, and that is where it is set.
#[inline]
pub fn jit_tlab_registration_required() -> bool {
    JIT_TLAB_REGISTRATION_REQUIRED.load(std::sync::atomic::Ordering::Acquire)
}

/// Publish the value [`jit_tlab_registration_required`] reports.
#[inline]
pub fn set_jit_tlab_registration_required(required: bool) {
    JIT_TLAB_REGISTRATION_REQUIRED.store(required, std::sync::atomic::Ordering::Release);
}

static JIT_TLAB_REGISTRATION_REQUIRED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Corrupt-`Value`-cell census, shared by the three crates that need it.
///
/// It lives HERE rather than in the collector or the VM because the only exit
/// path that every program takes — the shutdown trailer in
/// `native-builtins::lang_system` — can reach `cratonvm-types` and neither of
/// the other two. The first version printed its summary from `vm-cli`'s
/// normal-return arm, and a JUnit runner calls `System.exit`, so a full
/// 1975-class Spring Boot sweep produced the line in ZERO of 1975 logs while
/// looking exactly like a clean run.
///
/// That is the failure this module exists to make impossible: `decoded=0` is a
/// measurement, an ABSENT line is not, and the two must not look alike.
/// Post-remap stale-frame-word census, printed at exit while the detector is
/// armed (`CRATONVM_DBG_JIT_STALE_AFTER_REMAP`).
///
/// Lives here for the same reason `cell_census` does: the shutdown trailer that
/// a `System.exit`ing program actually reaches is in `native-builtins`, which
/// cannot call into `vm` where the detector lives. The counters are fed by
/// `vm::jit::conservative_roots::report_stale_words_in`.
///
/// The SPLIT is the measurement. `resumed_from` counts words in a compiled
/// frame's callee-saved GPR image — the save area whose epilogue pops it
/// straight back into the CALLER's registers, so the caller resumes from
/// exactly those words. `dead_region` counts the rest of what the frame-band
/// verifier skips (the XMM image, the write-only per-safepoint GPR spill, the
/// outgoing-argument / deopt reserve, operand slots above the live cursor);
/// nothing loads from those, so a stale word there is read by no one and is
/// deliberately left alone. A run reporting `resumed_from=0 dead_region=N` is
/// the repaired state, not a quiet one.
pub mod stale_remap_census {
    use std::sync::atomic::{AtomicU64, Ordering};

    static RESUMED: AtomicU64 = AtomicU64::new(0);
    static DEAD: AtomicU64 = AtomicU64::new(0);

    /// Count one stale word. `resumed_from` says whether anything reads it.
    #[inline]
    pub fn note(resumed_from: bool) {
        if resumed_from {
            RESUMED.fetch_add(1, Ordering::Relaxed);
        } else {
            DEAD.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// `(resumed_from, dead_region)` totals for this process.
    #[inline]
    pub fn totals() -> (u64, u64) {
        (
            RESUMED.load(Ordering::Relaxed),
            DEAD.load(Ordering::Relaxed),
        )
    }

    /// Print the census once, on whichever exit path runs first. Zeros
    /// included — see the module comment.
    pub fn exit_summary() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        if crate::flags::runtime_var_os("CRATONVM_DBG_JIT_STALE_AFTER_REMAP").is_none() {
            return;
        }
        ONCE.call_once(|| {
            let (r, d) = totals();
            eprintln!("[jit-stale-after-remap] census: resumed_from={r} dead_region={d}");
        });
    }
}

/// Did `CRATONVM_SCALAR_DEOPT` actually DO anything on this run?
///
/// The flag's whole effect is one predicate — `deopt_descriptor_available` in
/// `jit`'s `plan_scalar_replacement`. An allocation escape analysis has already
/// proved replaceable is DELETED when a deopt snapshot naming it can be handed
/// a recipe, and KEPT when it cannot. Nothing else changes: the analysis, the
/// loads it forwards and the stores it kills are identical either way.
///
/// So a gauntlet that reports "green with the flag on" says nothing until it
/// also says the flag reached the workload. `rescued` is the flag's engagement
/// count — allocations deleted that the default path keeps — and `blocked` is
/// the same population seen from the OTHER arm, which is what makes a zero
/// readable: `rescued=0 blocked=0` means the workload has no allocation in this
/// shape at all and the arm proves nothing, while `rescued=0 blocked=N` would
/// mean the flag was on and still could not describe them.
///
/// Reported from the `System.exit` path for the same reason
/// [`cell_census::exit_summary`] is: a JUnit runner never reaches `vm-cli`'s
/// normal-return arm, and a census printed there produces zero lines across a
/// whole suite sweep.
pub mod scalar_deopt_census {
    use std::sync::atomic::{AtomicU64, Ordering};

    static RESCUED: AtomicU64 = AtomicU64::new(0);
    static BLOCKED: AtomicU64 = AtomicU64::new(0);
    static MATERIALIZED: AtomicU64 = AtomicU64::new(0);

    /// One allocation elided BECAUSE a deopt descriptor was available for it —
    /// i.e. one the default path would have kept.
    #[inline]
    pub fn note_rescued() {
        RESCUED.fetch_add(1, Ordering::Relaxed);
    }

    /// One allocation proved replaceable and kept anyway, because no deopt
    /// descriptor was available for it. This is what the OFF arm counts.
    #[inline]
    pub fn note_blocked() {
        BLOCKED.fetch_add(1, Ordering::Relaxed);
    }

    /// One RUNTIME reconstruction of scalar-replaced objects — a deopt that
    /// actually had to rebuild what the compiler deleted.
    ///
    /// This is the engagement number that matters most, and it is a different
    /// question from `rescued`. `rescued` says the compiler took the flag's
    /// path; only this says the RECIPE was executed. A soak with `rescued=N`
    /// and `materialized=0` has proved that deleting those allocations does not
    /// break the program — it has NOT tested the descriptor that exists to put
    /// them back, because nothing asked for them back.
    #[inline]
    pub fn note_materialized(n: u64) {
        if n != 0 {
            MATERIALIZED.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// `(rescued, blocked, materialized)`.
    #[inline]
    pub fn totals() -> (u64, u64, u64) {
        (
            RESCUED.load(Ordering::Relaxed),
            BLOCKED.load(Ordering::Relaxed),
            MATERIALIZED.load(Ordering::Relaxed),
        )
    }

    /// One line on the exit path, UNCONDITIONAL when either counter is non-zero.
    ///
    /// Not behind a debug flag, and deliberately: the numbers are two relaxed
    /// atomics bumped once per scalar-replacement plan (a compile-time event,
    /// not a per-execution one), so the cost of always having them is nil, and
    /// the alternative — an engagement census you have to know to ask for — is
    /// how a soak gets run without one.
    pub fn exit_summary() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let (r, b, m) = totals();
        if r == 0 && b == 0 && m == 0 {
            return;
        }
        ONCE.call_once(|| {
            eprintln!("[scalar-deopt] census: rescued={r} blocked={b} materialized={m}");
        });
    }
}

/// Per-launch driver-call bookkeeping the GPU bridge saved, and did not.
///
/// Two savings landed together on 2026-08-29 and neither is visible from a
/// wall clock on this host: a `CUevent` free list, so a kernel submission
/// stops paying `cuEventCreate` now and `cuEventDestroy` later for each of
/// the two events it mints, and a same-stream wait elision, so a launch
/// stops issuing `cuStreamWaitEvent` for an ordering its own stream
/// already guarantees. GPULlama3 makes 453 submissions per token, so the
/// two are worth roughly 1,800 driver calls a token -- but a change that
/// never fires looks exactly like one that fires and does not help, and
/// this box's clock moves enough between two runs to hide either.
///
/// So: count. `recycled` against `created` says whether the pool is
/// serving anything; `elided` against `issued` says how much of the
/// ordering traffic was a stream waiting on itself.
///
/// The counters live here rather than in `cuda-bridge` for the same reason
/// [`cell_census`] does: they are written in one crate and printed in
/// another, and this is the crate both of them already depend on.
/// How much of the GPU dispatch path is being served from memo rather than
/// re-derived.
///
/// The two memos this counts -- the per-call-site method resolution and the
/// `craton/gpu/GpuArray` class id -- have no observable semantics, so a
/// wall clock on a shared host cannot tell "it did not fire" from "it fired
/// and did not help". These counters can.
///
/// A `resolutions_missed` that keeps climbing after warm-up is the finding:
/// it means call sites are not repeating, and the memo is pure overhead for
/// that workload.
/// Where the time in one TRANSPARENT (`--gpu`) offload dispatch goes.
///
/// # Why this is not `craton_gpu::dispatch_timing`
///
/// That module measures `submitMethod` — the explicit `GpuExecutor` API —
/// and its `CALLS` counter only moves there. The `--gpu` auto-offload
/// path never goes through it, so every transparent run printed no phase
/// table at all, and the per-dispatch floor that dominates every small
/// kernel (~48 us on an RTX 2060) had never been broken down. Two rounds
/// of plausible guessing at that floor bought 117 us -> 100 us, which is
/// what guessing usually buys. This is the same instrument for the other
/// door.
///
/// Off unless `CRATONVM_GPU_TIME_DISPATCH=1` (the same switch, because it
/// is the same question); the counters are plain relaxed atomics and the
/// report prints at exit beside the other censuses.
///
/// `total` is the whole of `try_dispatch` and CONTAINS every other phase.
/// Read the parts against it: what it holds beyond their sum is dispatch
/// overhead none of them names, which is the thing worth finding.
/// Where the time goes in a transparent dispatch that REFUSES.
///
/// # Why the dispatch table cannot answer this
///
/// [`gpu_offload_phase_census`] counts a call only once it is committed to
/// the device (`note_call` sits at the point of no return), so every
/// fall-through is invisible to it. Fall-throughs are the common case and,
/// on a CPU-bound workload, the expensive one: `GpuHookOverheadBench`
/// measures a cached `invokestatic` at 279 ns and the same site with the
/// hook refusing at 10,122 ns -- a 9.8 us refusal, 36x the call it
/// decorates. kfusion's CPU path runs 8x slower under `--gpu` for exactly
/// this reason, offloading nothing.
///
/// A site whose target is ELIGIBLE is deliberately kept out of the
/// per-call-site invoke cache so a later call with bigger arrays can still
/// offload, so it re-enters the hook forever. That is the design; paying
/// 9.8 us for it is not.
///
/// Off unless `CRATONVM_GPU_TIME_DISPATCH=1`, the same switch as its twin.
/// How many methods `--gpu` denied JIT admission.
///
/// AUDIT 2026-09-03. `vm::runtime::offload_jit_gate` refuses JIT and OSR
/// admission to any method whose body contains an offload-eligible
/// `invokestatic`, so the interpreter hook can still see that site. The
/// consequence is that the WHOLE caller runs interpreted -- its loops, its
/// arithmetic, everything -- and that is the largest cost `--gpu` imposes on
/// a CPU-bound program. Measured on `GpuHookOverheadBench`, the same method:
///
/// ```text
///   no --gpu (JIT admitted)                            10 ns/call
///   --gpu, site promoted, hook reached only 514 times  6,139 ns/call
/// ```
///
/// 600x, against 0.48 us for the hook itself and 0.75 for the lost invoke
/// cache. Nothing counted it until this census existed.
/// WHICH call actually started a collection.
///
/// AUDIT 2026-09-03. `[GC] zgc-trigger` counts the four branches of
/// `needs_gc`, plus the arena's hard refusal. On kfusion under `--gpu` all
/// five read ZERO on a run that collected 13 times, so every one of those
/// cycles entered through a door none of them watches.
///
/// There are three doors, and none counted itself:
///
/// * `maybe_gc` — the allocation-path check. Fires on `needs_gc()` OR on
///   the `gc_requested` latch, and the latch is invisible to the trigger
///   tallies, so a cycle can start here with every trigger at zero.
/// * `maybe_gc_forced` — the safepoint's forced path, taken when the
///   boundary's gates say collect.
/// * `force_gc_from_native` — `System.gc()` / `Runtime.gc()`.
///
/// Counting the door is what separates "the heap decided" from "someone
/// asked", which is the question left after the trigger census came back
/// empty.
/// Why an OSR compile was refused, per gate.
///
/// AUDIT 2026-09-04. `compile_osr_artifact` sets `osr_stage("entry")` and
/// then has FOUR early gates that `return None` without setting a stage of
/// their own, so every early refusal reports `stage=entry` and the four are
/// indistinguishable. That is how a refusal can be real and unattributed:
/// `gpu_jit_gate_census` counts 7 methods blocked, all one-shot
/// `<clinit>`s, while `Integration.integrate` gets ZERO OSR enters under
/// `--gpu` and 14 other methods still enter -- so something refuses it that
/// nothing counts.
///
/// This distinguishes the two possibilities the gate census cannot:
/// OSR was ATTEMPTED and refused by a named gate, or OSR was never
/// attempted at all, in which case no gate ran and the method appears
/// nowhere.
pub mod osr_refusal_census {
    use std::sync::atomic::{AtomicU64, Ordering};

    static ATTEMPTS: AtomicU64 = AtomicU64::new(0);
    static REFUSALS: std::sync::Mutex<Vec<(&'static str, u64)>> =
        std::sync::Mutex::new(Vec::new());
    static NAMED: std::sync::Mutex<Vec<(String, &'static str)>> =
        std::sync::Mutex::new(Vec::new());
    const MAX_NAMED: usize = 48;

    /// One entry into `compile_osr_artifact`.
    #[inline]
    pub fn note_attempt() {
        ATTEMPTS.fetch_add(1, Ordering::Relaxed);
    }

    /// One refusal, tagged with the gate that returned `None`.
    pub fn note_refusal(gate: &'static str, method: &str) {
        let mut v = REFUSALS.lock().unwrap_or_else(|p| p.into_inner());
        match v.iter_mut().find(|(g, _)| *g == gate) {
            Some((_, n)) => *n += 1,
            None => v.push((gate, 1)),
        }
        drop(v);
        let mut n = NAMED.lock().unwrap_or_else(|p| p.into_inner());
        if n.len() < MAX_NAMED && !n.iter().any(|(m, g)| m == method && *g == gate) {
            n.push((method.to_string(), gate));
        }
    }

    pub fn exit_summary() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let attempts = ATTEMPTS.load(Ordering::Relaxed);
        if attempts == 0 {
            return;
        }
        ONCE.call_once(|| {
            let mut v = REFUSALS.lock().unwrap_or_else(|p| p.into_inner()).clone();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            let refused: u64 = v.iter().map(|(_, n)| n).sum();
            eprintln!(
                "[cratonvm] osr refusals: attempts={attempts} refused_at_early_gate={refused}"
            );
            for (gate, n) in v {
                eprintln!("[cratonvm] osr refusals:   {gate}: {n}");
            }
            for (m, g) in NAMED.lock().unwrap_or_else(|p| p.into_inner()).iter() {
                eprintln!("[cratonvm] osr refusals:   {g} <- {m}");
            }
        });
    }
}

pub mod gc_entry_census {
    use std::sync::atomic::{AtomicU64, Ordering};

    static MAYBE_GC_NEEDS: AtomicU64 = AtomicU64::new(0);
    static MAYBE_GC_REQUESTED: AtomicU64 = AtomicU64::new(0);
    static FORCED: AtomicU64 = AtomicU64::new(0);
    static FROM_NATIVE: AtomicU64 = AtomicU64::new(0);

    /// `maybe_gc` collected because `needs_gc()` said so.
    #[inline]
    pub fn note_maybe_gc_needs() {
        MAYBE_GC_NEEDS.fetch_add(1, Ordering::Relaxed);
    }

    /// `maybe_gc` collected because the `gc_requested` latch was set —
    /// the case no trigger tally can see.
    #[inline]
    pub fn note_maybe_gc_requested() {
        MAYBE_GC_REQUESTED.fetch_add(1, Ordering::Relaxed);
    }

    /// `maybe_gc_forced` ran, tagged with the call site that asked.
    ///
    /// `maybe_gc_forced_pub` has 24 call sites across seven files -- the
    /// safepoint gate, five in the JIT helpers, four on the deopt-resume
    /// path, and more -- so a bare count says a collection was FORCED
    /// without saying by whom, which is the question left when every
    /// trigger tally reads zero.
    #[inline]
    pub fn note_forced_at(site: &'static str) {
        FORCED.fetch_add(1, Ordering::Relaxed);
        let mut v = FORCED_SITES.lock().unwrap_or_else(|p| p.into_inner());
        match v.iter_mut().find(|(s, _)| *s == site) {
            Some((_, n)) => *n += 1,
            None => v.push((site, 1)),
        }
    }

    static FORCED_SITES: std::sync::Mutex<Vec<(&'static str, u64)>> =
        std::sync::Mutex::new(Vec::new());

    /// `(site, count)` for every forced collection, busiest first.
    pub fn forced_sites() -> Vec<(&'static str, u64)> {
        let mut v = FORCED_SITES
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v
    }

    /// `System.gc()` / `Runtime.gc()`.
    #[inline]
    pub fn note_from_native() {
        FROM_NATIVE.fetch_add(1, Ordering::Relaxed);
    }

    static REFILL_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
    static REFILL_SUCCESSES: AtomicU64 = AtomicU64::new(0);
    static ALLOC_TOTAL_AT_EXIT: AtomicU64 = AtomicU64::new(0);

    /// One `refill_tlab` call and whether it produced a chunk.
    ///
    /// The wedge break fires after 16,384 CONSECUTIVE failures, and the
    /// counter only resets on a success -- so "does refill ever succeed"
    /// decides whether the breaker is armed permanently or not at all. On
    /// ZGC `refill_tlab` is opt-in (`CRATONVM_ZGC_JIT_TLAB`), so the
    /// expectation is zero successes and the interesting number is how
    /// many bytes flow past it.
    #[inline]
    pub fn note_refill(success: bool) {
        REFILL_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
        if success {
            REFILL_SUCCESSES.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Publish `bytes_allocated_total` for the exit line. It is the wedge
    /// break's RE-ARM metric (one break per 64 MB), so it, not the
    /// collection count, is what sets how often the breaker can fire.
    pub fn note_alloc_total(bytes: u64) {
        ALLOC_TOTAL_AT_EXIT.store(bytes, Ordering::Relaxed);
    }

    static REFILL_RETRY_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
    static REFILL_RETRY_SUCCESSES: AtomicU64 = AtomicU64::new(0);

    /// The refill RETRY that follows a wedge break, and whether the forced
    /// collection actually bought a chunk.
    ///
    /// Counted apart from the first attempt because it is the only refill
    /// that can succeed on this backend: the first one is asking a feature
    /// that is off (`CRATONVM_ZGC_JIT_TLAB`), while the retry runs after a
    /// coalescing collection. If it succeeds, the TLAB it seeds serves
    /// allocations that bump `bytes_allocated_total`, which is the wedge
    /// break's own re-arm -- 64 MB later the breaker fires again. That is a
    /// LOOP, and this counter is what distinguishes it from a one-off.
    #[inline]
    pub fn note_refill_retry(success: bool) {
        REFILL_RETRY_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
        if success {
            REFILL_RETRY_SUCCESSES.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// `(retry_attempts, retry_successes)`.
    pub fn refill_retry_totals() -> (u64, u64) {
        (
            REFILL_RETRY_ATTEMPTS.load(Ordering::Relaxed),
            REFILL_RETRY_SUCCESSES.load(Ordering::Relaxed),
        )
    }

    /// `(attempts, successes, alloc_total)`.
    pub fn refill_totals() -> (u64, u64, u64) {
        (
            REFILL_ATTEMPTS.load(Ordering::Relaxed),
            REFILL_SUCCESSES.load(Ordering::Relaxed),
            ALLOC_TOTAL_AT_EXIT.load(Ordering::Relaxed),
        )
    }

    /// `(maybe_gc_needs, maybe_gc_requested, forced, from_native)`.
    pub fn totals() -> (u64, u64, u64, u64) {
        (
            MAYBE_GC_NEEDS.load(Ordering::Relaxed),
            MAYBE_GC_REQUESTED.load(Ordering::Relaxed),
            FORCED.load(Ordering::Relaxed),
            FROM_NATIVE.load(Ordering::Relaxed),
        )
    }
}

pub mod gpu_jit_gate_census {
    use std::sync::atomic::{AtomicU64, Ordering};

    static BLOCKED: AtomicU64 = AtomicU64::new(0);
    static ADMITTED: AtomicU64 = AtomicU64::new(0);

    /// One verdict, counted per DISTINCT method: the gate caches its
    /// verdicts, so this counts first judgements rather than consultations,
    /// which is the number that says how much of the program moved off the
    /// JIT.
    #[inline]
    pub fn note_verdict(blocked: bool) {
        if blocked {
            BLOCKED.fetch_add(1, Ordering::Relaxed);
        } else {
            ADMITTED.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Why a method was blocked. The two reasons are very different in
    /// reach, and the count alone cannot tell them apart.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum BlockReason {
        /// The method writes a primitive array, so compiling it would
        /// invalidate the input-residency cache with no hook to notice.
        /// This is the BROAD reason: it catches any numeric kernel that
        /// stores to an `int[]`/`long[]`/`float[]`/`double[]`, whether or
        /// not it has anything to do with offload.
        /// `CRATONVM_GPU_JIT_ARRAY_WRITERS=allow` trades the cache for the
        /// compilation instead.
        WritesPrimitiveArray,
        /// The method calls an offload-eligible `invokestatic`, so
        /// compiling it would hide that site from the interpreter hook.
        /// This is the NARROW reason, and the one the gate is named for.
        CallsEligibleKernel,
    }

    /// Names of the methods this gate denied, with the reason.
    ///
    /// A count says how MANY; only the names say whether the ones blocked
    /// are the ones a workload spends its time in. kfusion blocks 7 of 301
    /// methods -- 2.3%, which sounds negligible and would be the whole
    /// story if those 7 are its integration loop.
    ///
    /// Bounded: a program with thousands of blocked methods has a
    /// different problem, and the list is a diagnostic rather than a log.
    static BLOCKED_NAMES: std::sync::Mutex<Vec<(String, &'static str)>> =
        std::sync::Mutex::new(Vec::new());
    const MAX_NAMED: usize = 64;

    /// Record one blocked method by name. Called only on a first verdict.
    pub fn note_blocked_name(name: String, reason: BlockReason) {
        let mut v = BLOCKED_NAMES.lock().unwrap_or_else(|p| p.into_inner());
        if v.len() >= MAX_NAMED {
            return;
        }
        let tag = match reason {
            BlockReason::WritesPrimitiveArray => "writes-primitive-array",
            BlockReason::CallsEligibleKernel => "calls-eligible-kernel",
        };
        v.push((name, tag));
    }

    pub fn exit_summary() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let blocked = BLOCKED.load(Ordering::Relaxed);
        let admitted = ADMITTED.load(Ordering::Relaxed);
        if blocked + admitted == 0 {
            return;
        }
        ONCE.call_once(|| {
            eprintln!(
                "[cratonvm] gpu jit gate: methods judged={} blocked_from_jit={blocked}                  admitted={admitted} ({:.1}% denied JIT, so the whole caller runs                  interpreted and its offload sites stay visible to the hook)",
                blocked + admitted,
                100.0 * blocked as f64 / (blocked + admitted) as f64,
            );
            let names = BLOCKED_NAMES.lock().unwrap_or_else(|p| p.into_inner());
            for (name, reason) in names.iter() {
                eprintln!("[cratonvm] gpu jit gate:   blocked {name}  ({reason})");
            }
            if blocked as usize > names.len() {
                eprintln!(
                    "[cratonvm] gpu jit gate:   ... and {} more not listed",
                    blocked as usize - names.len()
                );
            }
        });
    }
}

pub mod gpu_refusal_census {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Phases, in the order `try_dispatch` runs them. `total` is the whole
    /// refusal and CONTAINS the rest.
    pub const PHASES: [&str; 5] = [
        // The class-manager read lock, the class-name hash, and the LINEAR
        // scan over the class's methods comparing two strings each.
        "resolve_method",
        // `OffloadCache::lookup_or_compile`.
        "lookup_kernel",
        // Descriptor shape, then `largest_primitive_array_len` against the
        // real arguments -- the only part that genuinely must run per call,
        // because the arrays can grow.
        "gates",
        // NESTED: the whole refusal.
        "total",
        // The caller's own guard in `dispatch_static`, which runs before
        // `try_dispatch` on every call at a hooked site.
        "hook_guard",
    ];
    pub const TOTAL: usize = 3;

    /// Why a refusal refused, in the order the checks run.
    pub const REASONS: [&str; 5] = [
        "class_not_loaded",
        "method_not_found",
        "not_offloadable",
        "descriptor_shape",
        "below_min_work",
    ];

    static NANOS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
    static COUNTS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

    pub fn enabled() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            crate::flags::runtime_var("CRATONVM_GPU_TIME_DISPATCH")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
        })
    }

    #[inline]
    pub fn add(phase: usize, nanos: u64) {
        if phase < NANOS.len() {
            NANOS[phase].fetch_add(nanos, Ordering::Relaxed);
        }
    }

    #[inline]
    pub fn note_refusal(reason: usize) {
        if reason < COUNTS.len() {
            COUNTS[reason].fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn exit_summary() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let refusals: u64 = COUNTS.iter().map(|c| c.load(Ordering::Relaxed)).sum();
        if refusals == 0 {
            return;
        }
        ONCE.call_once(|| {
            let total = NANOS[TOTAL].load(Ordering::Relaxed);
            eprintln!(
                "[cratonvm] gpu offload refusals: n={refusals} total={:.3} us/refusal",
                total as f64 / refusals as f64 / 1000.0
            );
            for (i, name) in REASONS.iter().enumerate() {
                let c = COUNTS[i].load(Ordering::Relaxed);
                if c == 0 {
                    continue;
                }
                eprintln!(
                    "[cratonvm] gpu offload refusals:   {name:<18} {c:>12} ({:.1}%)",
                    100.0 * c as f64 / refusals as f64
                );
            }
            let mut named = 0u64;
            for (i, name) in PHASES.iter().enumerate() {
                if i == TOTAL {
                    continue;
                }
                let n = NANOS[i].load(Ordering::Relaxed);
                named = named.saturating_add(n);
                eprintln!(
                    "[cratonvm] gpu offload refusals:   {name:<18} {:>8.3} us/refusal ({:.1}%)",
                    n as f64 / refusals as f64 / 1000.0,
                    100.0 * n as f64 / total.max(1) as f64
                );
            }
            eprintln!(
                "[cratonvm] gpu offload refusals:   {:<18} {:>8.3} us/refusal ({:.1}%)",
                "unaccounted",
                total.saturating_sub(named) as f64 / refusals as f64 / 1000.0,
                100.0 * total.saturating_sub(named) as f64 / total.max(1) as f64
            );
        });
    }
}

pub mod gpu_offload_phase_census {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Index into [`NANOS`]. `total` last so the parts read first.
    pub const PHASES: [&str; 11] = [
        // `try_dispatch`: class-manager lookup of the class and method.
        "resolve_method",
        // `OffloadCache::lookup_or_compile` — a hit after the first call.
        "lookup_kernel",
        // Descriptor shape + `--gpu-min-work` against the real array len.
        "gates",
        // `dispatch_method_inner` steps 4-5: device context and stream.
        "ctx_and_stream",
        // Steps 6-7: the GC-critical marshal window and the H2D uploads.
        "marshal_args",
        // Step 8: `launch_on_stream`.
        "launch",
        // Steps 1-3: the cache handle, the SECOND class+method resolve,
        // and the dispatch memo that exists to make it cheap.
        "dispatch_prologue",
        // Step 9 onward: building and registering the submission.
        "dispatch_epilogue",
        // `finalize_submission`'s `event.synchronize()` — the host
        // waiting for the DEVICE. Not overhead: a synchronous API owes
        // its caller a finished kernel. Read it as the floor the async
        // path exists to hide.
        "finalize_wait",
        // The rest of `finalize_submission`: the writeback window, the
        // D2H copies, the scalar download.
        "finalize_writeback",
        // NESTED: the whole of `try_dispatch`. Contains all of the above.
        "total",
    ];

    pub const TOTAL: usize = 10;

    /// Dispatches whose phases are NOT recorded.
    ///
    /// The first call through a method compiles it — analyze, lower,
    /// `ptxas`, module load — which is tens of milliseconds, and averaged
    /// over a run it lands entirely on `lookup_kernel`. At 2000
    /// dispatches that read 27.01 us/call and looked like a hash lookup
    /// gone wrong; at 20000 it read 0.35, which is what a hash lookup
    /// costs. The table is about the STEADY state, so the compile is
    /// excluded rather than smeared over it.
    const WARMUP_CALLS: u64 = 1;

    static NANOS: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];
    static CALLS: AtomicU64 = AtomicU64::new(0);

    pub fn enabled() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            crate::flags::runtime_var("CRATONVM_GPU_TIME_DISPATCH")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
        })
    }

    #[inline]
    pub fn add(phase: usize, nanos: u64) {
        // `note_call` has already counted this dispatch, so the first one
        // sees `CALLS == 1`. See [`WARMUP_CALLS`].
        if CALLS.load(Ordering::Relaxed) <= WARMUP_CALLS {
            return;
        }
        if phase < NANOS.len() {
            NANOS[phase].fetch_add(nanos, Ordering::Relaxed);
        }
    }

    /// One dispatch that reached the device. Counted at the point of no
    /// return, NOT at entry: `try_dispatch` is called for every eligible
    /// invokestatic and falls through on a cache miss, a wrong-shaped
    /// descriptor or a too-small array, and averaging the real dispatches
    /// over those would report a floor far below the real one.
    pub fn note_call() {
        CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub fn exit_summary() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let all = CALLS.load(Ordering::Relaxed);
        let calls = all.saturating_sub(WARMUP_CALLS);
        if calls == 0 {
            return;
        }
        ONCE.call_once(|| {
            let total = NANOS[TOTAL].load(Ordering::Relaxed);
            eprintln!(
                "[cratonvm] gpu offload dispatch: calls={calls} (of {all}, \
                 first {WARMUP_CALLS} excluded as compile) total={:.2} us/call",
                total as f64 / calls as f64 / 1000.0
            );
            let mut named = 0u64;
            for (i, name) in PHASES.iter().enumerate() {
                if i == TOTAL {
                    continue;
                }
                let n = NANOS[i].load(Ordering::Relaxed);
                named = named.saturating_add(n);
                if n == 0 {
                    continue;
                }
                eprintln!(
                    "[cratonvm] gpu offload dispatch:   {name:<15} {:>8.2} us/call \
                     ({:.1}%)",
                    n as f64 / calls as f64 / 1000.0,
                    100.0 * n as f64 / total.max(1) as f64,
                );
            }
            // The gap is the point of the table: it is the dispatch cost
            // that none of the phases above names, and it is where the
            // next change should be aimed.
            let gap = total.saturating_sub(named);
            eprintln!(
                "[cratonvm] gpu offload dispatch:   {:<15} {:>8.2} us/call ({:.1}%)",
                "unaccounted",
                gap as f64 / calls as f64 / 1000.0,
                100.0 * gap as f64 / total.max(1) as f64,
            );
        });
    }
}

pub mod gpu_dispatch_memo_census {
    use std::sync::atomic::{AtomicU64, Ordering};

    static RESOLVE_HIT: AtomicU64 = AtomicU64::new(0);
    static RESOLVE_MISS: AtomicU64 = AtomicU64::new(0);
    static ARRAY_PROBE: AtomicU64 = AtomicU64::new(0);

    /// One dispatch served from the resolution memo.
    #[inline]
    pub fn note_resolve_hit() {
        RESOLVE_HIT.fetch_add(1, Ordering::Relaxed);
    }

    /// One dispatch that had to resolve the long way.
    #[inline]
    pub fn note_resolve_miss() {
        RESOLVE_MISS.fetch_add(1, Ordering::Relaxed);
    }

    /// One argument tested against the cached `GpuArray` class id.
    #[inline]
    pub fn note_array_probe() {
        ARRAY_PROBE.fetch_add(1, Ordering::Relaxed);
    }

    /// `(resolution hits, resolution misses, array type probes)`.
    #[must_use]
    pub fn totals() -> (u64, u64, u64) {
        (
            RESOLVE_HIT.load(Ordering::Relaxed),
            RESOLVE_MISS.load(Ordering::Relaxed),
            ARRAY_PROBE.load(Ordering::Relaxed),
        )
    }

    /// One line on the exit path, when this process dispatched anything.
    pub fn exit_summary() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let (hit, miss, probes) = totals();
        if hit + miss == 0 {
            return;
        }
        ONCE.call_once(|| {
            eprintln!(
                "[cratonvm] gpu dispatch memo: resolutions served={hit} re-derived={miss} \
                 ({:.1}% memoised); GpuArray type probes={probes}, each one \
                 integer compare",
                100.0 * hit as f64 / (hit + miss).max(1) as f64,
            );
        });
    }
}

/// What the GPU input-residency cache did across garbage collections.
///
/// The cache is keyed by `ObjectRef` -- a raw heap address -- so every
/// collection has to re-key the entries whose arrays moved and drop the
/// ones whose arrays died. Until 2026-09-02 the ONLY account of that was
/// a `tracing::debug!` inside `input_cache::remap_and_sweep`, and
/// `tracing` is built here with `max_level_info`: the statement is
/// compiled out of every release build, so the path was unobservable in
/// any binary anyone actually runs. A test that tried to confirm the
/// remap carried the new `short[]`/`byte[]` entries read zero from it
/// and could not tell "nothing moved" from "nothing can be reported".
///
/// `gcs` counts collections seen by the cache including the ones where
/// it had nothing to do, so a zero in `rekeyed` can be read: no
/// collections at all, versus collections that never moved a cached
/// array.
/// What the GPU submission registry did over the run.
///
/// `offload::SUBMISSIONS` had exactly one insert and one remove, and the
/// remove had no production caller: every async submission stayed
/// registered for the life of the process. The only account of that was
/// a `tracing::warn!` fired once per doubling past 1024 live, which tells
/// you a threshold was crossed and never how many leaked, nor whether a
/// drain you just wired actually drains.
///
/// `live` at exit is the number that matters: on a program that releases
/// every handle it takes, it should be zero.
/// Whether the chunked (overlapped) writeback path was actually taken.
///
/// A chunking change is invisible to a value differential: the
/// whole-array writeback is correct too, so `marshal-stress` passes
/// identically whether chunking ran or silently never engaged. That is
/// how `short[]`/`byte[]` sat outside the chunkable set from the day they
/// became offloadable -- `take_chunkable_writeback` bailed on an array
/// writeback it did not recognise, which cost the WHOLE dispatch its
/// copy/compute overlap, and nothing reported it.
///
/// `refused` counts dispatches where a chunkable candidate existed but
/// the path was declined (no stream pool, a non-tiling length, a
/// `launch_chunked` error that fell back). `taken` and `refused` together
/// say whether the feature is doing anything at all.
pub mod gpu_chunk_census {
    use std::sync::atomic::{AtomicU64, Ordering};

    static TAKEN: AtomicU64 = AtomicU64::new(0);
    static REFUSED: AtomicU64 = AtomicU64::new(0);
    static CHUNKS: AtomicU64 = AtomicU64::new(0);

    /// One dispatch used the chunked writeback, split into `chunks`.
    #[inline]
    pub fn note_taken(chunks: u64) {
        TAKEN.fetch_add(1, Ordering::Relaxed);
        CHUNKS.fetch_add(chunks, Ordering::Relaxed);
    }

    /// One dispatch had a chunkable writeback and did not use the path.
    #[inline]
    pub fn note_refused() {
        REFUSED.fetch_add(1, Ordering::Relaxed);
    }

    /// `(taken, refused, total_chunks)`.
    #[must_use]
    pub fn totals() -> (u64, u64, u64) {
        (
            TAKEN.load(Ordering::Relaxed),
            REFUSED.load(Ordering::Relaxed),
            CHUNKS.load(Ordering::Relaxed),
        )
    }

    /// One line on the exit path, when anything was chunkable.
    pub fn exit_summary() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let (taken, refused, chunks) = totals();
        if taken + refused == 0 {
            return;
        }
        ONCE.call_once(|| {
            eprintln!(
                "[cratonvm] gpu chunked writeback: taken={taken} refused={refused} \
                 chunks={chunks}"
            );
        });
    }
}

pub mod gpu_submission_census {
    use std::sync::atomic::{AtomicU64, Ordering};

    static REGISTERED: AtomicU64 = AtomicU64::new(0);
    static RELEASED: AtomicU64 = AtomicU64::new(0);
    static PEAK: AtomicU64 = AtomicU64::new(0);

    /// One submission entered the registry; `live` is the table size
    /// after the insert.
    #[inline]
    pub fn note_register(live: u64) {
        REGISTERED.fetch_add(1, Ordering::Relaxed);
        PEAK.fetch_max(live, Ordering::Relaxed);
    }

    /// One entry was actually removed. Not counted for a release call
    /// naming a handle that was already gone -- the point is to measure
    /// drains that happened, not drains that were attempted.
    #[inline]
    pub fn note_release() {
        RELEASED.fetch_add(1, Ordering::Relaxed);
    }

    /// `(registered, released, peak_live)`.
    #[must_use]
    pub fn totals() -> (u64, u64, u64) {
        (
            REGISTERED.load(Ordering::Relaxed),
            RELEASED.load(Ordering::Relaxed),
            PEAK.load(Ordering::Relaxed),
        )
    }

    /// One line on the exit path, when this process registered anything.
    pub fn exit_summary() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let (registered, released, peak) = totals();
        if registered == 0 {
            return;
        }
        ONCE.call_once(|| {
            let live = registered.saturating_sub(released);
            eprintln!(
                "[cratonvm] gpu submissions: registered={registered} released={released} \
                 live_at_exit={live} peak_live={peak}"
            );
        });
    }
}

pub mod gpu_residency_census {
    use std::sync::atomic::{AtomicU64, Ordering};

    static GCS: AtomicU64 = AtomicU64::new(0);
    static REKEYED: AtomicU64 = AtomicU64::new(0);
    static RETAINED: AtomicU64 = AtomicU64::new(0);
    static DROPPED: AtomicU64 = AtomicU64::new(0);

    /// One collection's worth of remap accounting.
    #[inline]
    pub fn note_gc(rekeyed: u64, retained: u64, dropped: u64) {
        GCS.fetch_add(1, Ordering::Relaxed);
        if rekeyed != 0 {
            REKEYED.fetch_add(rekeyed, Ordering::Relaxed);
        }
        if retained != 0 {
            RETAINED.fetch_add(retained, Ordering::Relaxed);
        }
        if dropped != 0 {
            DROPPED.fetch_add(dropped, Ordering::Relaxed);
        }
    }

    /// `(collections, re-keyed, retained, dropped)`.
    #[must_use]
    pub fn totals() -> (u64, u64, u64, u64) {
        (
            GCS.load(Ordering::Relaxed),
            REKEYED.load(Ordering::Relaxed),
            RETAINED.load(Ordering::Relaxed),
            DROPPED.load(Ordering::Relaxed),
        )
    }

    /// One line on the exit path, when the cache saw any collection.
    pub fn exit_summary() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let (gcs, rekeyed, retained, dropped) = totals();
        if gcs == 0 {
            return;
        }
        ONCE.call_once(|| {
            eprintln!(
                "[cratonvm] gpu residency across GC: collections={gcs} \
                 entries re-keyed={rekeyed} retained={retained} dropped={dropped}"
            );
        });
    }
}

pub mod gpu_event_census {
    use std::sync::atomic::{AtomicU64, Ordering};

    static CREATED: AtomicU64 = AtomicU64::new(0);
    static RECYCLED: AtomicU64 = AtomicU64::new(0);
    static WAITS_ISSUED: AtomicU64 = AtomicU64::new(0);
    static WAITS_ELIDED: AtomicU64 = AtomicU64::new(0);
    static WAITS_ELIDED_LATCHED: AtomicU64 = AtomicU64::new(0);
    static ALLOC_HIT: AtomicU64 = AtomicU64::new(0);
    static ALLOC_MISS: AtomicU64 = AtomicU64::new(0);
    static ALLOC_PARKED: AtomicU64 = AtomicU64::new(0);

    /// One device allocation served from the bridge's allocation pool.
    #[inline]
    pub fn note_alloc_pool_hit() {
        ALLOC_HIT.fetch_add(1, Ordering::Relaxed);
    }

    /// One device allocation that had to go to `cuMemAlloc`.
    #[inline]
    pub fn note_alloc_pool_miss() {
        ALLOC_MISS.fetch_add(1, Ordering::Relaxed);
    }

    /// One freed device allocation parked in the pool instead of freed.
    #[inline]
    pub fn note_alloc_pool_parked() {
        ALLOC_PARKED.fetch_add(1, Ordering::Relaxed);
    }

    /// `(pool hits, cuMemAlloc calls, blocks parked)`.
    #[must_use]
    pub fn alloc_totals() -> (u64, u64, u64) {
        (
            ALLOC_HIT.load(Ordering::Relaxed),
            ALLOC_MISS.load(Ordering::Relaxed),
            ALLOC_PARKED.load(Ordering::Relaxed),
        )
    }

    /// One `cuEventCreate` the pool could not serve.
    #[inline]
    pub fn note_created() {
        CREATED.fetch_add(1, Ordering::Relaxed);
    }

    /// One event handed out from the free list instead of created.
    #[inline]
    pub fn note_recycled() {
        RECYCLED.fetch_add(1, Ordering::Relaxed);
    }

    /// One `cuStreamWaitEvent` actually issued.
    #[inline]
    pub fn note_wait_issued() {
        WAITS_ISSUED.fetch_add(1, Ordering::Relaxed);
    }

    /// One wait skipped because the event was recorded on the same stream.
    #[inline]
    pub fn note_wait_elided() {
        WAITS_ELIDED.fetch_add(1, Ordering::Relaxed);
    }

    /// One wait skipped because the event had ALREADY FIRED, rather than
    /// because it was recorded on the waiting stream.
    ///
    /// Counted separately from [`note_wait_elided`] because the two
    /// answer different questions. Same-stream elision says the caller
    /// kept a chain on one stream; this one says the caller waited on
    /// stale work — a resident buffer whose `last_write` nothing
    /// rewrites — and it is the counter that says whether the latch is
    /// earning its query.
    pub fn note_wait_elided_latched() {
        WAITS_ELIDED_LATCHED.fetch_add(1, Ordering::Relaxed);
        WAITS_ELIDED.fetch_add(1, Ordering::Relaxed);
    }

    /// `(created, recycled, waits issued, waits elided)`.
    #[must_use]
    pub fn totals() -> (u64, u64, u64, u64) {
        (
            CREATED.load(Ordering::Relaxed),
            RECYCLED.load(Ordering::Relaxed),
            WAITS_ISSUED.load(Ordering::Relaxed),
            WAITS_ELIDED.load(Ordering::Relaxed),
        )
    }

    /// One line on the exit path, when this process launched any kernel.
    ///
    /// Silent for a run with no GPU work at all -- there is nothing to
    /// report and every CPU-only test would otherwise grow a line -- but
    /// NOT silent for a run whose pool served nothing. `recycled=0` beside
    /// a large `created` is the finding, and a census you have to know to
    /// ask for is how a soak gets run without one.
    pub fn exit_summary() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let (created, recycled, issued, elided) = totals();
        let latched = WAITS_ELIDED_LATCHED.load(Ordering::Relaxed);
        let (alloc_hit, alloc_miss, alloc_parked) = alloc_totals();
        if created + recycled + issued + elided + alloc_hit + alloc_miss == 0 {
            return;
        }
        ONCE.call_once(|| {
            eprintln!(
                "[cratonvm] gpu events: created={created} recycled={recycled} \
                 (pool served {:.1}%); stream waits issued={issued} \
                 elided={elided} ({:.1}% elided, {latched} already-fired);                  device allocs: cuMemAlloc={alloc_miss} \
                 pooled={alloc_hit} ({:.1}% pooled) parked={alloc_parked}",
                100.0 * recycled as f64 / (created + recycled).max(1) as f64,
                100.0 * elided as f64 / (issued + elided).max(1) as f64,
                100.0 * alloc_hit as f64 / (alloc_hit + alloc_miss).max(1) as f64,
            );
        });
    }
}

pub mod cell_census {
    use std::sync::atomic::{AtomicU64, Ordering};

    static DECODED: AtomicU64 = AtomicU64::new(0);
    static REPORTED: AtomicU64 = AtomicU64::new(0);
    static ARRAY_RECEIVER: AtomicU64 = AtomicU64::new(0);

    /// Count one corrupt cell decoded. Returns the count BEFORE this one, which
    /// is what a watch compares against.
    #[inline]
    pub fn note_decoded() -> u64 {
        DECODED.fetch_add(1, Ordering::Relaxed)
    }

    /// How many corrupt cells this process has decoded.
    #[inline]
    pub fn decoded() -> u64 {
        DECODED.load(Ordering::Relaxed)
    }

    /// Count one corrupt cell actually NAMED — by a read door or the backstop.
    #[inline]
    pub fn note_reported() {
        REPORTED.fetch_add(1, Ordering::Relaxed);
    }

    /// How many corrupt cells this process has named.
    #[inline]
    pub fn reported() -> u64 {
        REPORTED.load(Ordering::Relaxed)
    }

    /// Count one plain-object field access refused because the RECEIVER was an
    /// array — the same defect one step EARLIER than a corrupt cell.
    ///
    /// Counted separately because it is a different measurement. The corrupt
    /// cell is what you see when the striden element bytes happen to form an
    /// out-of-range discriminant; this is what you see EVERY time, which is why
    /// `decoded` was a floor and this is a count. See
    /// `gc::heap::refuse_array_receiver_field_access`.
    #[inline]
    pub fn note_array_receiver() {
        ARRAY_RECEIVER.fetch_add(1, Ordering::Relaxed);
    }

    /// How many array-receiver field accesses this process has refused.
    #[inline]
    pub fn array_receiver() -> u64 {
        ARRAY_RECEIVER.load(Ordering::Relaxed)
    }

    /// Print the census once, on whichever exit path runs first.
    ///
    /// Armed by `CRATONVM_DBG_CORRUPT_CELL`. Prints even when nothing fired —
    /// that is the point: it turns "the instrument said nothing" into "the
    /// instrument was armed and counted zero", which are different claims.
    pub fn exit_summary() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        if crate::flags::runtime_var_os("CRATONVM_DBG_CORRUPT_CELL").is_none() {
            return;
        }
        ONCE.call_once(|| {
            let d = decoded();
            let r = reported();
            let a = array_receiver();
            // Always printed, `0` included, and always beside `decoded`: these
            // are the same defect at two removes, and a run with `decoded=0
            // array_receiver=7` has found seven producers the corrupt-cell
            // guard could not see. `decoded` counts these too — the refusal
            // publishes the cell coordinates so the producer reporter names the
            // door — so `decoded - array_receiver` is the number of cells that
            // came through some OTHER route.
            eprintln!("[corrupt-cell] array_receiver={a}");
            if d == 0 {
                eprintln!(
                    "[corrupt-cell] decoded=0 reported=0 — armed, and the guard did not fire"
                );
            } else if r < d {
                eprintln!(
                    "[corrupt-cell] decoded={d} reported={r} — {} cell(s) NAMED BY NOBODY: a \
                     reader with no instrumented door whose thread never reached a safepoint \
                     afterwards",
                    d - r
                );
            } else {
                eprintln!("[corrupt-cell] decoded={d} reported={r}");
            }
        });
    }
}
