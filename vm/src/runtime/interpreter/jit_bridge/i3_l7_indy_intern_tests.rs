//! `interned_jit_indy_bridge_site`: one site per `(class, generation, cp
//! index, instruction)` per VM, instead of one leaked allocation per
//! compile attempt.
use super::*;
use crate::classloading::{Class, ClassLoaderId, ClassState};
use crate::config::VmConfig;
use cratonvm_reader::attribute::BootstrapMethod;
use cratonvm_reader::class_access_flags::ClassAccessFlags;
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

/// cp #10 is an `invokedynamic` whose bootstrap is
/// `StringConcatFactory.makeConcat`; cp #5 is not an indy at all.
fn concat_class(id: ClassId) -> Class {
    let utf8 = |s: &str| ConstantPoolEntry::Utf8(s.into());
    let pool = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        utf8("java/lang/invoke/StringConcatFactory"), // 1
        ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
        utf8("makeConcat"),                           // 3
        utf8(
            "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;\
             Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/CallSite;",
        ), // 4
        ConstantPoolEntry::NameAndType {
            name_index: 3,
            descriptor_index: 4,
        }, // 5
        ConstantPoolEntry::MethodReference {
            class_index: 2,
            name_and_type_index: 5,
        }, // 6
        ConstantPoolEntry::MethodHandle {
            reference_kind: 6,
            reference_index: 6,
        }, // 7
        utf8("(I)Ljava/lang/String;"),                // 8
        ConstantPoolEntry::NameAndType {
            name_index: 3,
            descriptor_index: 8,
        }, // 9
        ConstantPoolEntry::InvokeDynamic {
            bootstrap_method_attr_index: 0,
            name_and_type_index: 9,
        }, // 10
    ]);
    Class {
        id,
        loader_id: ClassLoaderId::Application,
        name: cratonvm_types::intern_arc("cratonvm/test/IndyInternOwner"),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Initialized,
        initializing_thread: None,
        constant_pool: pool,
        access_flags: ClassAccessFlags::empty(),
        superclass: None,
        interfaces: vec![],
        fields: vec![],
        methods: vec![],
        first_field_index: 0,
        num_total_fields: 0,
        bootstrap_methods: vec![BootstrapMethod {
            bootstrap_method_ref: 7,
            bootstrap_arguments: vec![],
        }],
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
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
    }
}

/// Every compile attempt used to build (and leak) a fresh site. Now the
/// second, third, ... ask is answered with the first one's pointer, a
/// non-indy index is memoised as `0`, and another VM builds its own.
#[test]
fn a_bridge_site_is_built_once_per_instruction_per_vm() {
    let a = SharedVm::new(VmConfig::default());
    let b = SharedVm::new(VmConfig::default());
    let id = ClassId::new(0x7a11);
    let class = concat_class(id);
    let at = |bci: usize| Some(("m", "()Ljava/lang/String;", bci));
    let (first, again, not_indy, other_instruction) = {
        let cm = a.classes.class_manager.read();
        (
            interned_jit_indy_bridge_site(&a, &cm, &class, id, 10, at(3)),
            interned_jit_indy_bridge_site(&a, &cm, &class, id, 10, at(3)),
            interned_jit_indy_bridge_site(&a, &cm, &class, id, 5, at(3)),
            interned_jit_indy_bridge_site(&a, &cm, &class, id, 10, at(9)),
        )
    };
    assert_ne!(first, 0, "a makeConcat indy is bridged");
    assert_eq!(first, again, "the second attempt reuses the first site");
    assert_eq!(not_indy, 0);
    // Per INSTRUCTION, not per CP entry: a second instruction sharing the
    // entry gets its own site (a lambda site keys its singleton on it).
    assert_ne!(other_instruction, 0);
    assert_ne!(other_instruction, first);
    // SAFETY: `first` is a site pointer the bridge built above.
    assert_eq!(
        unsafe { crate::runtime::invokedynamic::jit_indy_site_kind(first) },
        crate::runtime::invokedynamic::JIT_INDY_SITE_CONCAT
    );
    assert_eq!(
        a.jit
            .bridge
            .indy_bridge_sites
            .lock()
            .map(|m| m.len())
            .unwrap_or(0),
        3
    );
    let other_vm = {
        let cm = b.classes.class_manager.read();
        interned_jit_indy_bridge_site(&b, &cm, &class, id, 10, at(3))
    };
    assert_ne!(other_vm, 0);
    assert_ne!(other_vm, first, "the table is per VM");
}

/// `CpResolvers::indy` names the instruction from the bytecode only when
/// exactly one `invokedynamic` of the method uses the entry.
#[test]
fn the_sole_indy_bci_names_one_instruction_or_none() {
    // invokedynamic #10; areturn
    let one = [0xba, 0x00, 0x0a, 0x00, 0x00, 0xb0];
    assert_eq!(sole_indy_bci_of(&one, 10), Some(0));
    assert_eq!(sole_indy_bci_of(&one, 11), None);
    // aload_0; invokedynamic #10; pop; invokedynamic #10; areturn
    let two = [
        0x2a, 0xba, 0x00, 0x0a, 0x00, 0x00, 0x57, 0xba, 0x00, 0x0a, 0x00, 0x00, 0xb0,
    ];
    assert_eq!(sole_indy_bci_of(&two, 10), None);
    // aload_0; invokedynamic #11; pop; invokedynamic #10; areturn
    let mixed = [
        0x2a, 0xba, 0x00, 0x0b, 0x00, 0x00, 0x57, 0xba, 0x00, 0x0a, 0x00, 0x00, 0xb0,
    ];
    assert_eq!(sole_indy_bci_of(&mixed, 10), Some(7));
    assert_eq!(sole_indy_bci_of(&mixed, 11), Some(1));
    // sipush 0xba00; lconst_1; ireturn -- the `0xba` is an operand byte.
    let operand = [0x11, 0xba, 0x00, 0x0a, 0xac];
    assert_eq!(sole_indy_bci_of(&operand, 10), None);
    assert_eq!(sole_indy_bci_of(&[], 10), None);
}

/// JIT round 16 wave 10 (lane indyir): `CpResolvers::indy_at` names an
/// instruction only at a bci that holds an `invokedynamic` of that entry --
/// each of two instructions sharing an entry is named on its own.
#[test]
fn r16w10_the_indy_operand_is_read_only_at_an_invokedynamic() {
    // aload_0; invokedynamic #10; pop; invokedynamic #10; areturn
    let two = [
        0x2a, 0xba, 0x00, 0x0a, 0x00, 0x00, 0x57, 0xba, 0x00, 0x0a, 0x00, 0x00, 0xb0,
    ];
    assert_eq!(indy_operand_at(&two, 1), Some(10));
    assert_eq!(indy_operand_at(&two, 7), Some(10));
    assert_eq!(indy_operand_at(&two, 0), None, "aload_0");
    assert_eq!(indy_operand_at(&two, 6), None, "pop");
    assert_eq!(indy_operand_at(&two, 13), None, "past the end");
    assert_eq!(indy_operand_at(&[0xba, 0x00], 0), None, "truncated");
}

/// The by-name callee door's identity (`i1-L7-callee-compile-doors-are-loader-blind`):
/// a hint is used only while it names a loaded class of that name; a stale,
/// zero or foreign hint falls back to the by-name answer.
#[test]
fn the_callee_probe_honours_only_a_hint_that_names_the_class() {
    let shared = SharedVm::new(VmConfig::default());
    let (a, b) = {
        let mut cm = shared.classes.class_manager.write();
        (
            cm.try_ensure_synthetic_class("cratonvm/test/CalleeHintA", 0)
                .expect("Compatible mode fabricates"),
            cm.try_ensure_synthetic_class("cratonvm/test/CalleeHintB", 0)
                .expect("Compatible mode fabricates"),
        )
    };
    let cm = shared.classes.class_manager.read();
    let by_name = cm
        .get_loaded_class_id("cratonvm/test/CalleeHintA")
        .unwrap_or(ClassId::new(0));
    assert_eq!(
        callee_probe_class_id(&cm, Some(a), "cratonvm/test/CalleeHintA"),
        a
    );
    // A hint naming a different class is ignored.
    assert_eq!(
        callee_probe_class_id(&cm, Some(b), "cratonvm/test/CalleeHintA"),
        by_name
    );
    assert_eq!(
        callee_probe_class_id(&cm, Some(ClassId::new(0)), "cratonvm/test/CalleeHintA"),
        by_name
    );
    assert_eq!(
        callee_probe_class_id(&cm, None, "cratonvm/test/CalleeHintA"),
        by_name
    );
    assert_eq!(
        callee_probe_class_id(&cm, None, "cratonvm/test/NotLoadedAnywhere"),
        ClassId::new(0)
    );
}
