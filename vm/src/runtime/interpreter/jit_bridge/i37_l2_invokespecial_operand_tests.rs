use super::{CpResolvers, SubstitutedOwners};
use crate::config::VmConfig;
use crate::vm::SharedVm;

#[test]
fn only_an_invokespecial_instructions_operand_is_named() {
    let shared = SharedVm::new(VmConfig::default());
    // aload_0; invokespecial #7; aload_0; invokevirtual #9;
    // sipush 0xb700 (its operand bytes read `b7 00 57` from pc 9);
    // pop; return; two bytes of padding.
    let code = [
        0x2a, 0xb7, 0x00, 0x07, 0x2a, 0xb6, 0x00, 0x09, 0x11, 0xb7, 0x00, 0x57, 0x57, 0xb1,
        0x00, 0x00,
    ];
    let cp = CpResolvers {
        shared: &shared,
        class_id: cratonvm_types::ClassId::new(0),
        substituted_owners: SubstitutedOwners::default(),
        method_name: "run",
        method_descriptor: "()V",
        code: &code,
        ldc2w_trace: None,
        invoke_operands: Default::default(),
        first_calls_seen: Default::default(),
        deferred_fields_seen: Default::default(),
    };
    assert!(cp.names_an_invoke_of(7, 0xb7));
    assert!(!cp.names_an_invoke_of(9, 0xb7), "an invokevirtual operand");
    assert!(
        !cp.names_an_invoke_of(0x0057, 0xb7),
        "the bytes after sipush's 0xb7 operand are not an instruction"
    );
    assert!(
        cp.names_an_invoke_of(7, 0xb7),
        "answered again from the memo"
    );
}
