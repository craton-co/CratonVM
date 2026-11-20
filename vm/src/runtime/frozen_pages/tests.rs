// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;
use crate::classloading::{Class, ClassState};
use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
use cratonvm_reader::class_access_flags::{ClassAccessFlags, FieldAccessFlags, MethodAccessFlags};
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry, CpUtf8};
use cratonvm_reader::field::ClassFileField;
use cratonvm_reader::method::ClassFileMethod;
use std::sync::atomic::AtomicU8;

fn make_field(name: &str, desc: &str, flags: FieldAccessFlags) -> ClassFileField {
    ClassFileField {
        access_flags: flags,
        name: Arc::from(name),
        descriptor: Arc::from(desc),
        attributes: Vec::new(),
    }
}

fn make_method(name: &str, desc: &str, code: Vec<u8>) -> ClassFileMethod {
    ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC,
        name: Arc::from(name),
        descriptor: Arc::from(desc),
        attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack: 4,
                max_locals: 4,
                code: cratonvm_reader::ByteView::from_vec(code),
                exception_table: Vec::new(),
                attributes: Vec::new(),
            },
        )))],
    }
}

fn make_class(
    id: ClassId,
    name: &str,
    superclass: Option<ClassId>,
    cp: ConstantPool,
    fields: Vec<ClassFileField>,
    methods: Vec<ClassFileMethod>,
    first_field_index: usize,
    num_total_fields: usize,
) -> Class {
    Class {
        id,
        loader_id: ClassLoaderId::Application,
        name: Arc::from(name),
        source_file: None,
        version: ClassFileVersion { major: 65, minor: 0 },
        state: ClassState::Initialized,
        initializing_thread: None,
        constant_pool: cp,
        access_flags: ClassAccessFlags::from_bits_truncate(0x0021),
        superclass,
        interfaces: Vec::new(),
        fields,
        methods,
        first_field_index,
        num_total_fields,
        bootstrap_methods: Vec::new(),
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
        has_finalizer: false,
        code_source: None,
        array_info: None,
        record_object_methods: AtomicU8::new(0),
        init_state: Arc::new(AtomicU8::new(2)),
    }
}

fn single_field_cp(owner: &str, fname: &str, fdesc: &str) -> ConstantPool {
    let utf8 = |s: &str| ConstantPoolEntry::Utf8(CpUtf8::from_arc(Arc::from(s)));
    ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        utf8(owner),
        ConstantPoolEntry::ClassReference { name_index: 1 },
        utf8(fname),
        utf8(fdesc),
        ConstantPoolEntry::NameAndType {
            name_index: 3,
            descriptor_index: 4,
        },
        // Index 6:
        ConstantPoolEntry::FieldReference {
            class_index: 2,
            name_and_type_index: 5,
        },
    ])
}

#[test]
fn cha_distinguishes_this_ctor_init_from_other_receiver_and_non_init_putfield() {
    let vm = SharedVm::new(crate::config::VmConfig::default());
    let (clean_id, peer_write_id, mut_method_id) = {
        let mut cm = vm.classes.class_manager_write();
        // 1. Clean: `<init>(int v)` writes `this.x = v` (`aload_0; iload_1; putfield #6; return`)
        let cp1 = single_field_cp("test/Clean", "x", "I");
        let id1 = cm.class_store.next_id();
        cm.class_store.add(make_class(
            id1,
            "test/Clean",
            None,
            cp1,
            vec![make_field("x", "I", FieldAccessFlags::empty())],
            vec![make_method(
                "<init>",
                "(I)V",
                vec![0x2a, 0x1b, 0xb5, 0x00, 0x06, 0xb1],
            )],
            0,
            1,
        ));

        // 2. PeerWrite: `<init>(PeerWrite other)` writes `other.x = 2` (`aload_1; iconst_2; putfield #6; return`)
        let cp2 = single_field_cp("test/PeerWrite", "x", "I");
        let id2 = cm.class_store.next_id();
        cm.class_store.add(make_class(
            id2,
            "test/PeerWrite",
            None,
            cp2,
            vec![make_field("x", "I", FieldAccessFlags::empty())],
            vec![make_method(
                "<init>",
                "(Ltest/PeerWrite;)V",
                vec![0x2b, 0x05, 0xb5, 0x00, 0x06, 0xb1],
            )],
            0,
            1,
        ));

        // 3. MutMethod: `<init>` writes `this.x`, `bump()` writes `this.x` outside `<init>`.
        let cp3 = single_field_cp("test/MutMethod", "x", "I");
        let id3 = cm.class_store.next_id();
        cm.class_store.add(make_class(
            id3,
            "test/MutMethod",
            None,
            cp3,
            vec![make_field("x", "I", FieldAccessFlags::empty())],
            vec![
                make_method("<init>", "(I)V", vec![0x2a, 0x1b, 0xb5, 0x00, 0x06, 0xb1]),
                make_method("bump", "()V", vec![0x2a, 0x04, 0xb5, 0x00, 0x06, 0xb1]),
            ],
            0,
            1,
        ));
        (id1, id2, id3)
    };

    ensure_cha_caught_up(&vm);
    let (_, clean_plans) = compute_class_field_plans(&vm, clean_id);
    let (_, peer_plans) = compute_class_field_plans(&vm, peer_write_id);
    let (_, mut_plans) = compute_class_field_plans(&vm, mut_method_id);

    assert_eq!(clean_plans.len(), 1, "this.x in <init> is CHA-effectively-final");
    assert!(clean_plans[0].cha_final);
    assert!(peer_plans.is_empty(), "other.x in <init> must mark field mutable");
    assert!(mut_plans.is_empty(), "putfield outside <init> must mark field mutable");
}

#[test]
fn dynamic_class_define_and_slot_write_retire_folded_consumers_and_clear_facts() {
    cratonvm_types::flags::with_thread_overrides(&[("CRATONVM_FROZEN_PAGES", Some("1"))], || {
        let vm = SharedVm::new(crate::config::VmConfig::default());
        let box_id = {
            let mut cm = vm.classes.class_manager_write();
            let cp = single_field_cp("test/Box", "val", "I");
            let id = cm.class_store.next_id();
            cm.class_store.add(make_class(
                id,
                "test/Box",
                None,
                cp,
                vec![
                    make_field(
                        "holder",
                        "Ltest/Box;",
                        FieldAccessFlags::STATIC | FieldAccessFlags::FINAL,
                    ),
                    make_field("val", "I", FieldAccessFlags::empty()),
                ],
                vec![make_method(
                    "<init>",
                    "(I)V",
                    vec![0x2a, 0x1b, 0xb5, 0x00, 0x06, 0xb1],
                )],
                0,
                1,
            ));
            id
        };
        let box_obj = vm.mem.heap.try_alloc_object(box_id, 1).expect("alloc Box");
        vm.mem.heap.set_field(box_obj, 0, Value::Int(123));
        crate::vm::set_static_shared(&vm, box_id, 0, Value::Object(Some(box_obj)));
        vm.classes
            .static_final_ref_classes
            .write()
            .insert((box_id.as_u32(), 0), box_id.as_u32());

        freeze_graphs(&vm, &[box_obj]);
        refresh_facts_if_due(&vm);

        let facts = vm.classes.frozen.facts.read().clone().expect("initial snapshot");
        let root_obj_id = *facts.roots.get(&(box_id.as_u32(), 0)).expect("root filed");
        assert_eq!(
            facts.objects[root_obj_id as usize].fields,
            vec![Some(FrozenValue::Int(123))]
        );

        // Install a dummy compiled consumer recording `cratonvm/cha_field/test/Box`.
        let reader_cid = ClassId::new(999);
        let buf = cratonvm_jit::ExecutableBuffer::new(64).expect("exec buf");
        let mut cm_body = cratonvm_jit::CompiledMethod::new(buf);
        cm_body.inlined_methods.push((
            format!("{CHA_FIELD_DEP_PREFIX}test/Box"),
            "<cha-field>".to_string(),
            "()V".to_string(),
        ));
        vm.jit.jit_cache.write().put(
            Arc::from("test/Reader"),
            Arc::from("read"),
            Arc::from("()I"),
            reader_cid,
            cm_body,
        );
        assert!(vm
            .jit
            .jit_cache
            .read()
            .get("test/Reader", "read", "()I", reader_cid)
            .is_some());

        // Dynamically define `test/Mutator` containing `putfield test/Box.val:I`.
        let mutator_id = {
            let mut cm = vm.classes.class_manager_write();
            let cp = single_field_cp("test/Box", "val", "I");
            let id = cm.class_store.next_id();
            cm.class_store.add(make_class(
                id,
                "test/Mutator",
                None,
                cp,
                Vec::new(),
                vec![make_method(
                    "mutate",
                    "(Ltest/Box;)V",
                    vec![0x2a, 0x07, 0xb5, 0x00, 0x06, 0xb1],
                )],
                0,
                0,
            ));
            id
        };
        note_class_defined(&vm, mutator_id);

        assert!(
            vm.jit
                .jit_cache
                .read()
                .get("test/Reader", "read", "()I", reader_cid)
                .is_none(),
            "dynamic class with putfield test/Box.val must immediately evict compiled CHA consumer"
        );
        assert!(vm.classes.frozen.facts.read().is_none(), "stale snapshot cleared");

        // Re-snapshot must NO LONGER fold `test/Box.val`!
        vm.classes.frozen.last_refresh_end_us.store(0, Ordering::Relaxed);
        refresh_facts_if_due(&vm);
        let facts2 = vm.classes.frozen.facts.read().clone().expect("second snapshot");
        let root_obj_id2 = *facts2.roots.get(&(box_id.as_u32(), 0)).expect("root filed");
        assert!(
            facts2.objects[root_obj_id2 as usize].fields.is_empty(),
            "test/Box.val must not fold after Mutator was loaded"
        );
    });
}

#[test]
fn reflective_or_native_slot_write_invalidates_final_field_on_snapshotted_object_only() {
    cratonvm_types::flags::with_thread_overrides(&[("CRATONVM_FROZEN_PAGES", Some("1"))], || {
        let vm = SharedVm::new(crate::config::VmConfig::default());
        let imm_id = {
            let mut cm = vm.classes.class_manager_write();
            let cp = single_field_cp("test/Imm", "secret", "I");
            let id = cm.class_store.next_id();
            cm.class_store.add(make_class(
                id,
                "test/Imm",
                None,
                cp,
                vec![
                    make_field(
                        "INSTANCE",
                        "Ltest/Imm;",
                        FieldAccessFlags::STATIC | FieldAccessFlags::FINAL,
                    ),
                    make_field("secret", "I", FieldAccessFlags::FINAL),
                ],
                vec![make_method(
                    "<init>",
                    "(I)V",
                    vec![0x2a, 0x1b, 0xb5, 0x00, 0x06, 0xb1],
                )],
                0,
                1,
            ));
            id
        };
        let snapped = vm.mem.heap.try_alloc_object(imm_id, 1).expect("alloc snapped");
        let fresh = vm.mem.heap.try_alloc_object(imm_id, 1).expect("alloc fresh");
        vm.mem.heap.set_field(snapped, 0, Value::Int(55));
        crate::vm::set_static_shared(&vm, imm_id, 0, Value::Object(Some(snapped)));
        vm.classes
            .static_final_ref_classes
            .write()
            .insert((imm_id.as_u32(), 0), imm_id.as_u32());

        freeze_graphs(&vm, &[snapped]);
        refresh_facts_if_due(&vm);

        let imm_reader_cid = ClassId::new(998);
        let buf = cratonvm_jit::ExecutableBuffer::new(64).expect("exec buf");
        let mut body = cratonvm_jit::CompiledMethod::new(buf);
        body.inlined_methods.push((
            format!("{CHA_FIELD_DEP_PREFIX}test/Imm"),
            "<cha-field>".to_string(),
            "()V".to_string(),
        ));
        vm.jit.jit_cache.write().put(
            Arc::from("test/ImmReader"),
            Arc::from("get"),
            Arc::from("()I"),
            imm_reader_cid,
            body,
        );

        // Initializing a brand-new un-snapshotted object (`force_all_instances = false`) keeps consumer live!
        note_slot_write(&vm, fresh, 0, false);
        assert!(vm
            .jit
            .jit_cache
            .read()
            .get("test/ImmReader", "get", "()I", imm_reader_cid)
            .is_some());

        // Reflective `Field.set` on the snapshotted singleton immediately evicts consumer!
        note_slot_write(&vm, snapped, 0, false);
        assert!(vm
            .jit
            .jit_cache
            .read()
            .get("test/ImmReader", "get", "()I", imm_reader_cid)
            .is_none());
    });
}
