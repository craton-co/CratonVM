// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Shared types for the CratonVM project.
//!
//! This crate contains foundational types used by all subsystems:
//! value representation, class identifiers, heap layout constants,
//! and error types.

pub mod access_flags;
mod class_id;
pub mod compact_value;
pub mod compat;
pub mod error;
pub mod fdlibm;
pub mod field_layout;
pub mod field_watch;
pub mod flag_groups;
pub mod flags;
pub mod jfp;
pub mod float_format;
pub mod handle;
mod heap_types;
pub mod intern;
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
pub type PointerMap = rustc_hash::FxHashMap<usize, usize>;

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
    is_compact_object, layout_generation, layout_replace_guard, object_body_size,
    foreign_layout_refusals, next_layout_domain, pack_fields_by_width_enabled,
    read_compact_field, register_class_layout,
    set_compact_ref_fields_enabled, set_pack_fields_by_width_enabled, FIRST_LAYOUT_DOMAIN,
    unregister_class_layout, with_class_layout, write_compact_field, CompactLayout,
    FieldStorageKind,
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
pub use subsystem_config::{
    capability as capability_config, gc_metrics as gc_metrics_config,
    jit_metrics as jit_metrics_config, jit_verify as jit_verify_config, subsystems,
    thread_stress as thread_stress_config, CapabilityConfig, GcMetricsConfig, JitMetricsConfig,
    JitVerifyConfig, SubsystemConfig, ThreadStressConfig,
};
pub use float_format::{java_double_to_string, java_float_to_string};
pub use handle::{HandleScope, HandleStorage, RootedHandle};
// `mod heap_types` is private, so this list is the *only* way anything outside
// this crate can name a heap constant. A `pub const` added to `heap_types.rs`
// and left off this list is not merely inconvenient — it is unreachable from
// every other crate, i.e. an accidental default-off landing. `MARK_FORWARDED`,
// `FORWARDING_PTR_MASK` and `IDENTITY_HASH_CODE_OFFSET` were in exactly that
// state until 2026-07-26; see `arch-2026-07-26/header-shrink.md` §6.2 and the
// `every_public_heap_constant_is_reachable` test below.
pub use heap_types::{
    array_data_size, array_data_size_checked, array_element_type_from_tag, element_byte_size,
    element_type_tag_at, kind_tag_at,
    object_kind_from_tag, ArrayElementType, ObjectHeader, ObjectKind, ARRAY_DATA_OFFSET,
    ARRAY_LENGTH_OFFSET, AUTOBOX_CLASS_ID, FIELD_CELL_PAYLOAD32_OFFSET,
    FIELD_CELL_PAYLOAD64_OFFSET, FIELD_CELL_TAG_OBJECT, FIELD_CELL_TAG_OFFSET, FORWARDING_PTR_MASK,
    GC_FLAG_COMPACT, GC_FLAG_MARKED, GC_FLAG_OLD_GEN, HEADER_SIZE,
    GC_FLAGS_BYTE_OFFSET, KIND_TAGS_BYTE_OFFSET, KIND_TAG_BYTE_MASK,
    MARK_HASH_MASK, MARK_HASH_SHIFT, MARK_QUARTET_MASK, MARK_QUARTET_SHIFT, MAX_GC_AGE,
    INFLATED_PTR_MASK, MARK_FORWARDED, MARK_INFLATED, MARK_NEUTRAL,
    MARK_STATE_MASK, MARK_THIN_LOCKED, MARK_WORD_OFFSET, NUM_SLOTS_OFFSET,
    REF_ELEMENT_SIZE, REF_FIELD_SIZE, SLOT_SIZE, THIN_LOCK_OWNER_MASK, THIN_LOCK_OWNER_SHIFT,
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
        (RESUMED.load(Ordering::Relaxed), DEAD.load(Ordering::Relaxed))
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
                eprintln!("[corrupt-cell] decoded=0 reported=0 — armed, and the guard did not fire");
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
