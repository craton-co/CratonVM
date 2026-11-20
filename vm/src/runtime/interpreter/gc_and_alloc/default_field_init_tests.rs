//! G56-1 — the premise `init_primitive_fields` used to rest on, and the
//! behaviours the new reference arm must not move.
//!
//! The heap tests pin `GcAlgorithm::Generational` deliberately, exactly as
//! `root_snapshot_screen_tests` does and for a related reason: the claim
//! under test is about the LEGACY 16-byte `Value` cell's decode rule, which
//! is a property of `gen_heap::read_slot`. Whether ZGC's and G1's own slot
//! encodings answer the same way is a real and separate question, and this
//! fixture cannot ask it.

use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::vm::SharedVm;
use cratonvm_types::ClassId;

fn generational_vm() -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    })
}

/// The JVMS §2.3 default table, spelled out. Byte for byte, so a future
/// edit that (say) folds `Z` in with `J` is red here and not in a vector.
#[test]
fn every_jvm_field_descriptor_gets_its_spec_default() {
    for b in *b"IBCSZ" {
        assert_eq!(
            jvm_default_for_descriptor(b),
            Value::Int(0),
            "the int family (JVMS 2.3.1) all live in Value::Int"
        );
    }
    assert_eq!(jvm_default_for_descriptor(b'J'), Value::Long(0));
    assert_eq!(jvm_default_for_descriptor(b'F'), Value::Float(0.0));
    assert_eq!(jvm_default_for_descriptor(b'D'), Value::Double(0.0));
    // The arm this record exists for. Before 2026-08-17 both of these
    // answered "no write needed" and the slot kept its raw zero.
    assert_eq!(
        jvm_default_for_descriptor(b'L'),
        Value::Object(None),
        "a reference field's default is null, and null has to be WRITTEN"
    );
    assert_eq!(
        jvm_default_for_descriptor(b'['),
        Value::Object(None),
        "an array field is a reference field"
    );
}

/// The whole byte space, against the `gc` crate's own table.
///
/// `alloc_object_with_descriptors` is the other allocation entry point that
/// writes defaults, and it composes `heap::default_value_for_descriptor(b)`
/// with `.unwrap_or(Value::Object(None))`. If the two ever disagree, an
/// object's field defaults depend on which allocator ran — the class of
/// divergence this record was opened to close. Sweeping all 256 bytes also
/// covers the malformed-descriptor fall-open, which
/// `f.descriptor.as_bytes().first()` can genuinely produce.
#[test]
fn the_two_allocation_entry_points_agree_on_all_256_descriptor_bytes() {
    for b in 0u8..=255 {
        let theirs =
            cratonvm_gc::heap::default_value_for_descriptor(b).unwrap_or(Value::Object(None));
        assert_eq!(
            jvm_default_for_descriptor(b),
            theirs,
            "descriptor byte {b:#04x}: init_primitive_fields and \
             alloc_object_with_descriptors must not disagree"
        );
    }
}

/// The expired premise, asserted directly.
///
/// The comment this record removes said reference slots were "already
/// Object(None) from zero memory". `Value` is `#[repr(u32)]` with `Int = 0`
/// and `Object = 4`, so the all-zero cell decodes as `Int(0)`. If the first
/// assertion below ever fails in the direction of null, the niche was
/// reverted and the reference write becomes redundant rather than
/// load-bearing — which is worth being told.
#[test]
fn zero_memory_does_not_decode_as_null_which_is_why_the_write_exists() {
    let shared = generational_vm();
    let heap = &shared.mem.heap;
    let obj = heap.alloc_object(ClassId::new(0), 2);

    assert_eq!(
        heap.get_field(obj, 0),
        Value::Int(0),
        "a freshly allocated, never-written slot reads as Int(0) -- the \
         R-niche decode rule (gen_heap::read_slot), and the entire reason \
         1,105 of 1,120 instrument events existed"
    );
    assert_ne!(
        heap.get_field(obj, 0),
        Value::Object(None),
        "if this ever passes, the zero-bits-are-null shortcut is back"
    );

    heap.set_field(obj, 0, Value::Object(None));
    assert_eq!(
        heap.get_field(obj, 0),
        Value::Object(None),
        "and an EXPLICIT null is a different bit pattern that reads back as \
         null -- so the two states are distinguishable, and writing one is \
         not a no-op"
    );
}

/// ...and the fix changes no answer, which is why it is safe.
///
/// The descriptor-aware read is the one that was reporting the loss. Both
/// slot shapes answer `Object(None)` through it; only one of them fires the
/// G30 instrument on the way. That is the whole delta: signal, not
/// behaviour.
#[test]
fn a_raw_zero_and_an_explicit_null_read_identically_through_the_descriptor() {
    let shared = generational_vm();
    let heap = &shared.mem.heap;
    let obj = heap.alloc_object(ClassId::new(0), 2);

    // slot 0: left as the allocator produced it (the BEFORE state).
    // slot 1: written the way init_primitive_fields now writes it (AFTER).
    heap.set_field(obj, 1, Value::Object(None));

    for (slot, what) in [(0usize, "raw zero"), (1usize, "explicit null")] {
        for desc in *b"L[" {
            assert_eq!(
                heap.get_field_as(obj, slot, desc),
                Value::Object(None),
                "{what} at a '{}' slot must read as null either way",
                desc as char,
            );
        }
    }
}

/// `gc_alloc_object` answers `has_finalizer` and writes every instance
/// default under ONE class-manager read acquisition (it used to take two).
/// Pins the two halves that merge must not lose: the per-descriptor
/// defaults across a superclass chain (statics skipped, superclass slots
/// first), and the finalizer registration.
#[test]
fn gc_alloc_object_writes_every_default_and_registers_the_finalizable() {
    use crate::classloading::{Class, ClassLoaderId, ClassState};
    use cratonvm_reader::class_access_flags::{ClassAccessFlags, FieldAccessFlags};
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    use cratonvm_reader::field::ClassFileField;

    let shared = generational_vm();
    let field = |name: &str, desc: &str, flags: FieldAccessFlags| ClassFileField {
        access_flags: flags,
        name: std::sync::Arc::from(name),
        descriptor: std::sync::Arc::from(desc),
        attributes: vec![],
    };
    let add = |name: &str,
               superclass: Option<ClassId>,
               fields: Vec<ClassFileField>,
               first_field_index: usize,
               num_total_fields: usize,
               has_finalizer: bool| {
        let mut cm = shared.classes.class_manager.write();
        let id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: cratonvm_types::intern_arc(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Initialized,
            initializing_thread: None,
            constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
            access_flags: ClassAccessFlags::empty(),
            superclass,
            interfaces: vec![],
            fields,
            methods: vec![],
            first_field_index,
            num_total_fields,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: cratonvm_classloading::ClassOrigin::default(),
            has_finalizer,
            code_source: None,
            array_info: None,
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        });
        cm.register_class_name(ClassLoaderId::Application, name, id);
        id
    };
    let inst = FieldAccessFlags::PRIVATE;
    let stat = FieldAccessFlags::PRIVATE | FieldAccessFlags::STATIC;
    // Super: slots 0..2. A static in the middle must not take a slot.
    let base = add(
        "cratonvm/test/AllocDefaultsBase",
        None,
        vec![
            field("i", "I", inst),
            field("s", "J", stat),
            field("o", "Ljava/lang/Object;", inst),
        ],
        0,
        2,
        false,
    );
    // Sub: slots 2..6, and it overrides `finalize()`.
    let sub = add(
        "cratonvm/test/AllocDefaultsSub",
        Some(base),
        vec![
            field("j", "J", inst),
            field("f", "F", inst),
            field("d", "D", inst),
            field("a", "[I", inst),
        ],
        2,
        6,
        true,
    );

    let mut thread = JvmThread::new(crate::ThreadId(0), "test");
    let obj = gc_alloc_object(&shared, &mut thread, sub, 6).expect("a fresh heap allocates");
    let heap = &shared.mem.heap;
    assert_eq!(heap.get_field(obj, 0), Value::Int(0), "base.i");
    assert_eq!(heap.get_field(obj, 1), Value::Object(None), "base.o");
    assert_eq!(heap.get_field(obj, 2), Value::Long(0), "sub.j");
    assert_eq!(heap.get_field(obj, 3), Value::Float(0.0), "sub.f");
    assert_eq!(heap.get_field(obj, 4), Value::Double(0.0), "sub.d");
    assert_eq!(heap.get_field(obj, 5), Value::Object(None), "sub.a");
    assert!(
        finalizable_roots(&shared).contains(&(obj.as_ptr() as usize)),
        "a class with `has_finalizer` must register every instance"
    );

    let plain = gc_alloc_object(&shared, &mut thread, base, 2).expect("allocates");
    assert!(
        !finalizable_roots(&shared).contains(&(plain.as_ptr() as usize)),
        "a class without a finalizer must not be registered"
    );

    // Warm: with the allocation-recipe cache on, the first `new` of `sub`
    // published its recipe and this one is answered from it, lock-free.
    // Same defaults, same registration.
    let again = gc_alloc_object(&shared, &mut thread, sub, 6).expect("allocates");
    // `base.i` is NOT stored on the warm path (an int-family default is the
    // zeroed body itself); it must still read back as `Int(0)`.
    assert_eq!(heap.get_field(again, 0), Value::Int(0), "warm base.i");
    assert_eq!(heap.get_field(again, 1), Value::Object(None), "warm base.o");
    assert_eq!(heap.get_field(again, 2), Value::Long(0), "warm sub.j");
    assert_eq!(heap.get_field(again, 3), Value::Float(0.0), "warm sub.f");
    assert_eq!(heap.get_field(again, 4), Value::Double(0.0), "warm sub.d");
    assert_eq!(heap.get_field(again, 5), Value::Object(None), "warm sub.a");
    assert!(finalizable_roots(&shared).contains(&(again.as_ptr() as usize)));
    if crate::jit::alloc_class_cache::alloc_class_cache_enabled() {
        use crate::jit::alloc_class_cache::PrimKind;
        let info = shared
            .jit
            .jit_alloc_class_cache
            .get(sub.as_u32())
            .expect("the first `new` publishes the recipe");
        assert!(info.has_finalizer);
        // Walk order: the class itself, then its superclass; statics skipped.
        assert_eq!(
            &*info.prim_inits,
            &[
                (2, PrimKind::Long),
                (3, PrimKind::Float),
                (4, PrimKind::Double),
                (0, PrimKind::Int)
            ]
        );
        assert_eq!(&*info.ref_inits, &[5, 1]);
    }
}

/// The premise of `compact_body_holds_defaults` (lock-free `new` stage 3,
/// wave 16): at every compact storage kind, the recipe's default for that
/// kind is written as zero bytes, and zero bytes read back as that
/// default. So skipping the stores on a zeroed compact body changes no
/// byte. If a storage kind ever gains a non-zero default encoding (a
/// null that is not 0), this fails and the skip must go.
#[test]
fn compact_zero_body_already_holds_every_recipe_default() {
    use crate::jit::alloc_class_cache::PrimKind;
    use cratonvm_types::FieldStorageKind as K;
    use std::sync::atomic::Ordering;
    for desc in *b"ZBCSIJFDL[" {
        let storage = K::from_descriptor_byte(desc).expect("a field descriptor byte");
        // The default the recipe (`ClassAllocInfo::build`) assigns.
        let default =
            PrimKind::of_descriptor(desc).map_or(Value::Object(None), PrimKind::default_value);
        // One naturally aligned, zeroed slot of up to 8 bytes.
        let mut cell = [0u64; 1];
        let ptr = cell.as_mut_ptr() as *mut u8;
        // SAFETY: `ptr` is an 8-byte-aligned, 8-byte, exclusively owned
        // buffer, which covers every storage kind's size and alignment.
        let read =
            unsafe { cratonvm_types::read_compact_field(ptr, storage, Ordering::Relaxed) };
        assert_eq!(
            read, default,
            "zero bytes at {storage:?} must read as the default"
        );
        // SAFETY: as above.
        unsafe {
            cratonvm_types::write_compact_field(ptr, storage, default, Ordering::Relaxed)
        };
        assert_eq!(
            cell[0], 0,
            "the default at {storage:?} (descriptor '{}') must be stored as zero bytes",
            desc as char
        );
    }
}

/// `compact_body_holds_defaults` answers from the reserved size exactly as
/// `shape_of_reserved` stamps the header: a legacy reservation keeps the
/// stores, a smaller (compact) one skips them.
#[test]
fn only_a_compact_reservation_skips_the_default_stores() {
    use cratonvm_gc::heap::{HEADER_SIZE, SLOT_SIZE};
    let legacy = HEADER_SIZE + 3 * SLOT_SIZE;
    assert!(!compact_body_holds_defaults(3, legacy));
    // A zero-field object reads as legacy, and has nothing to store.
    assert!(!compact_body_holds_defaults(0, HEADER_SIZE));
    // A compact body packs each field to at most 8 bytes after an 8-byte
    // header: strictly smaller than the legacy reservation.
    assert_eq!(
        compact_body_holds_defaults(3, 8 + 3 * 8),
        COMPACT_BODY_DEFAULT_SKIP_ENABLED
    );
}

/// JVMS 6.5 `new`: an interface or abstract class answers
/// `InstantiationError` with HotSpot's external-name message; a concrete
/// class, and a compatibility stub whatever its guessed flags, answer
/// nothing.
#[test]
fn new_refuses_an_abstract_or_interface_class_but_not_a_stub() {
    use crate::classloading::{Class, ClassLoaderId, ClassState};
    use cratonvm_classloading::ClassOrigin;
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

    let class = |name: &str, access_flags: ClassAccessFlags, origin: ClassOrigin| Class {
        id: ClassId::new(1),
        loader_id: ClassLoaderId::Application,
        name: cratonvm_types::intern_arc(name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Loaded,
        initializing_thread: None,
        constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
        access_flags,
        superclass: None,
        interfaces: vec![],
        fields: vec![],
        methods: vec![],
        first_field_index: 0,
        num_total_fields: 0,
        bootstrap_methods: vec![],
        signature: None,
        annotations: Vec::new(),
        nest_host: None,
        nest_members: Vec::new(),
        record_components: Vec::new(),
        permitted_subclasses: Vec::new(),
        inner_classes: Vec::new(),
        enclosing_method: None,
        hidden: false,
        module_name: None,
        origin,
        has_finalizer: false,
        code_source: None,
        array_info: None,
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
    };
    let real = ClassOrigin::default;
    let stub = || ClassOrigin::CompatibilityStub {
        reason: std::sync::Arc::from("test"),
    };
    let concrete = ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER;
    let abstract_ =
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER | ClassAccessFlags::ABSTRACT;
    let interface =
        ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT;

    assert_eq!(
        new_instantiation_refusal(&class("p/q/Concrete", concrete, real())),
        None
    );
    assert_eq!(
        new_instantiation_refusal(&class("p/q/Abs", abstract_, real())),
        Some(("java/lang/InstantiationError", "p.q.Abs".to_string()))
    );
    assert_eq!(
        new_instantiation_refusal(&class("p/q/Iface", interface, real())),
        Some(("java/lang/InstantiationError", "p.q.Iface".to_string()))
    );
    assert_eq!(
        new_instantiation_refusal(&class("p/q/Guessed$Iface", interface, stub())),
        None,
        "a stub's flags are guessed and must not refuse a `new`"
    );
}

/// `HashMap.table` must still degrade to null.
///
/// Pinned in `gc/src/heap.rs` by
/// `the_hashmap_table_degrade_to_null_is_pinned` against the test-only
/// `Heap`; this is the same three values through the LIVE `VmHeap`
/// dispatch, so the guarantee is also asserted on the path a running VM
/// takes. `HashMap.resize()` reads `(oldTab == null) ? 0 : oldTab.length`,
/// so refusing or boxing the store breaks resize outright.
#[test]
fn the_hashmap_table_degrade_to_null_still_holds_through_vmheap() {
    let shared = generational_vm();
    let heap = &shared.mem.heap;
    let map = heap.alloc_object(ClassId::new(0), 3);

    for capacity in [Value::Int(16), Value::Int(1), Value::Long(64)] {
        heap.set_field_as(map, 2, capacity, b'[');
        assert_eq!(
            heap.get_field(map, 2),
            Value::Object(None),
            "a capacity written at an array-descriptor slot must degrade to \
             null; {capacity:?} did not"
        );
        assert_eq!(heap.get_field_as(map, 2, b'['), Value::Object(None));
    }
}

/// The `Int(1)` enqueued sentinel must survive default-initialisation.
///
/// `Reference.isEnqueued` was just repaired to accept BOTH the synthetic
/// `Int(1)` sentinel and a live `ReferenceQueue.ENQUEUED` object (G49-1
/// §4). The GC's auto-enqueue in this file writes that sentinel through the
/// RAW setter, and default-initialisation now writes `Object(None)` into
/// the same slot — earlier, at allocation. This pins the ordering: the
/// sentinel is written second and wins, and a reference that was never
/// enqueued reads as null rather than as the sentinel.
#[test]
fn the_enqueued_int_sentinel_outlives_the_default_null_written_at_alloc() {
    let shared = generational_vm();
    let heap = &shared.mem.heap;
    let reference = heap.alloc_object(ClassId::new(0), 3);

    // What init_primitive_fields now does for `queue : LReferenceQueue;`.
    heap.set_field(reference, 1, jvm_default_for_descriptor(b'L'));
    assert_ne!(
        heap.get_field(reference, 1),
        Value::Int(1),
        "a never-enqueued reference must not read as enqueued"
    );

    // What the post-GC auto-enqueue above does.
    heap.set_field(reference, 1, Value::Int(1));
    assert_eq!(
        heap.get_field(reference, 1),
        Value::Int(1),
        "the raw sentinel write must still win over the allocation-time \
         default -- un-fixing isEnqueued's synthetic arm is the failure \
         this guards"
    );
}
