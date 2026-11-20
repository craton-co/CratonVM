use super::*;
use cratonvm_reader::constant_pool::ConstantPoolEntry as CPE;

/// JIT round 21 lane indylate: an instruction the linked bridge may serve
/// reads "not linked" until its own row exists, and "linked" after; a
/// `void` site and a non-indy entry are never served (`None`), and another
/// instruction of the same entry has its own answer.
#[test]
fn an_instruction_reads_linked_only_once_its_own_row_exists() {
    let utf8 = |s: &str| CPE::Utf8(cratonvm_reader::constant_pool::CpUtf8::from_arc(Arc::from(s)));
    let pool = ConstantPool::new(vec![
        CPE::Tombstone,
        utf8("c2"),    // 1
        utf8("(II)I"), // 2
        CPE::NameAndType {
            name_index: 1,
            descriptor_index: 2,
        }, // 3
        CPE::InvokeDynamic {
            bootstrap_method_attr_index: 0,
            name_and_type_index: 3,
        }, // 4
        utf8("(I)V"),  // 5
        CPE::NameAndType {
            name_index: 1,
            descriptor_index: 5,
        }, // 6
        CPE::InvokeDynamic {
            bootstrap_method_attr_index: 0,
            name_and_type_index: 6,
        }, // 7
    ]);
    // A vm identity no real VM in this test process uses.
    let vm = usize::MAX - 0x21_1a7e;
    let class_id = ClassId::new(0x7_2101);
    let instr = ("k2", "(II)I", 12usize);
    let other = ("k2", "(II)I", 20usize);
    assert_eq!(
        jit_indy_instruction_linked(vm, &pool, class_id, 4, instr),
        Some(false),
        "not linked yet: the late bind"
    );
    assert_eq!(
        jit_indy_instruction_linked(vm, &pool, class_id, 7, instr),
        None
    );
    assert_eq!(
        jit_indy_instruction_linked(vm, &pool, class_id, 3, instr),
        None
    );
    let (method_key, pc) = lambda_singleton_site_for(instr.0, instr.1, instr.2);
    let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let gate = crate::classloading::resolution::RedefineGate::snapshot(Arc::clone(&counter));
    generic_indy_sites().write().insert(
        (vm, class_id, 4, method_key, pc),
        GenericIndySlot {
            // SAFETY: never dereferenced; the row is only probed.
            link: GenericIndyLink::Linked(unsafe { ObjectRef::from_raw(0x5_000 as *mut u8) }),
            shape: Arc::new(GenericIndyShape::new("(II)I")),
            gate,
        },
    );
    ANY_GENERIC_INDY_SITE.store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(
        jit_indy_instruction_linked(vm, &pool, class_id, 4, instr),
        Some(true)
    );
    assert_eq!(
        jit_indy_instruction_linked(vm, &pool, class_id, 4, other),
        Some(false),
        "another instruction of the entry links on its own"
    );
    forget_vm_generic_indy_sites(vm);
    assert_eq!(
        jit_indy_instruction_linked(vm, &pool, class_id, 4, instr),
        Some(false)
    );
}
