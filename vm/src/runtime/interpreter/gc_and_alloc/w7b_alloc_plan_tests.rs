//! gc-common round 2026-09-23: the `new` front end. Wave 7 (lane B7) put a
//! per-thread plan cache here behind a default-off flag; wave 9
//! retired it (the per-VM allocation recipe already serves every warm
//! `new`, so it measured no gain -- `applied/handoff-w9f-retire-the-alloc-plan-cache-flag.md`).
//! What stays: w9-f's stub-chain recipe test and B7's non-TLAB array charge.

use super::*;
use crate::classloading::{Class, ClassLoaderId, ClassState};
use crate::config::{GcAlgorithm, VmConfig};
use crate::vm::SharedVm;
use cratonvm_reader::class_access_flags::{ClassAccessFlags, FieldAccessFlags};
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use cratonvm_reader::field::ClassFileField;
use cratonvm_types::ClassId;

fn gen_vm() -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    })
}

fn field(name: &str, desc: &str, flags: FieldAccessFlags) -> ClassFileField {
    ClassFileField {
        access_flags: flags,
        name: std::sync::Arc::from(name),
        descriptor: std::sync::Arc::from(desc),
        attributes: vec![],
    }
}

#[allow(clippy::too_many_arguments)]
fn add_class(
    shared: &SharedVm,
    name: &str,
    superclass: Option<ClassId>,
    fields: Vec<ClassFileField>,
    first_field_index: usize,
    num_total_fields: usize,
    has_finalizer: bool,
    origin: cratonvm_classloading::ClassOrigin,
) -> ClassId {
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
        origin,
        has_finalizer,
        code_source: None,
        array_info: None,
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
    });
    cm.register_class_name(ClassLoaderId::Application, name, id);
    id
}

fn slots(shared: &SharedVm, obj: ObjectRef, n: usize) -> Vec<Value> {
    (0..n).map(|i| shared.mem.heap.get_field(obj, i)).collect()
}

/// gc-common w9-f: the allocation RECIPE (`jit::alloc_class_cache`), which
/// the interpreter's `new` consults before anything else, refuses a stub
/// chain for the reason the plan above does, so a `new` follows an
/// in-place stub upgrade.
///
/// The upgrade is staged as `upgrade_synthetic_class` performs it when the
/// synthetic field-count floor keeps the parent's total: same slot count,
/// the slot changes kind (`J` -> `L`), and no layout number of the
/// subclass moves, so nothing unpublishes the subclass's recipe. Before
/// the refusal the first `new` published `Sub`'s recipe with the stub's
/// `J`, and every later `new` wrote `Long(0)` into what is now a reference
/// slot.
#[test]
fn a_stub_chain_gets_no_recipe_so_new_follows_an_in_place_upgrade() {
    use crate::jit::alloc_class_cache::{alloc_class_cache_enabled, ClassAllocInfo};
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    let shared = gen_vm();
    let stub = add_class(
        &shared,
        "cratonvm/test/w9f/StubBase",
        None,
        vec![field("x", "J", FieldAccessFlags::PRIVATE)],
        0,
        1,
        false,
        cratonvm_classloading::ClassOrigin::CompatibilityStub {
            reason: std::sync::Arc::from("w9f test"),
        },
    );
    let sub = add_class(
        &shared,
        "cratonvm/test/w9f/SubOfStub",
        Some(stub),
        vec![field("r", "Ljava/lang/Object;", FieldAccessFlags::PRIVATE)],
        1,
        2,
        false,
        cratonvm_classloading::ClassOrigin::default(),
    );
    {
        let cm = shared.classes.class_manager.read();
        assert!(ClassAllocInfo::build(&cm.class_store, stub).is_none());
        assert!(
            ClassAllocInfo::build(&cm.class_store, sub).is_none(),
            "a stub anywhere in the chain refuses the recipe"
        );
    }
    let mut thread = JvmThread::new(ThreadId(0), "w9f-stub-chain");
    for _ in 0..2 {
        let obj =
            gc_alloc_object(&shared, &mut thread, sub, 2).expect("a fresh heap allocates");
        assert_eq!(
            slots(&shared, obj, 2),
            vec![Value::Long(0), Value::Object(None)],
            "the stub's layout, through the classic walk"
        );
    }
    assert!(
        shared.jit.jit_alloc_class_cache.get(sub.as_u32()).is_none(),
        "no recipe may be published for a stub chain"
    );

    // The in-place upgrade: the stub's `x` becomes a reference, the class
    // gets its real origin, and neither class's layout numbers move.
    {
        let mut cm = shared.classes.class_manager.write();
        let class = cm
            .class_store
            .get_mut(stub)
            .expect("the stub is in the store");
        class.fields = vec![field("x", "Ljava/lang/Object;", FieldAccessFlags::PRIVATE)];
        class.set_origin(cratonvm_classloading::ClassOrigin::default());
    }
    for _ in 0..2 {
        let obj = gc_alloc_object(&shared, &mut thread, sub, 2).expect("allocates");
        assert_eq!(
            slots(&shared, obj, 2),
            vec![Value::Object(None), Value::Object(None)],
            "a `new` after the upgrade writes the real layout's defaults"
        );
    }
    if alloc_class_cache_enabled() {
        let info = shared
            .jit
            .jit_alloc_class_cache
            .get(sub.as_u32())
            .expect("a stub-free chain is cached again");
        assert!(info.prim_inits.is_empty());
        assert_eq!(&*info.ref_inits, &[1, 0], "the class, then its superclass");
    }
}

/// An array too big for a TLAB is charged to the VM's
/// `bytes_allocated_total` as well as to the thread, exactly as an object
/// on the shared path is (`the_shared_object_path_charges_the_real_footprint`).
#[test]
fn a_non_tlab_array_is_charged_to_the_vm_allocation_total() {
    use crate::memory::heap::ArrayElementType;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use std::sync::atomic::Ordering;
    let shared = gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "w7b-array-total");
    let length = cratonvm_gc::tlab::tlab_max_alloc() * 2;
    let expected = external_array_bytes(ArrayElementType::Byte, length) as u64;
    let vm_before = shared.mem.bytes_allocated_total.load(Ordering::Relaxed);
    let thread_before = thread.tlab.thread_allocated_bytes();
    gc_alloc_array(
        &shared,
        &mut thread,
        ClassId::new(0),
        ArrayElementType::Byte,
        length,
    )
    .expect("an empty heap has room for one 64 KiB array");
    assert_eq!(
        shared.mem.bytes_allocated_total.load(Ordering::Relaxed) - vm_before,
        expected
    );
    assert_eq!(
        thread.tlab.thread_allocated_bytes() - thread_before,
        expected
    );
}
