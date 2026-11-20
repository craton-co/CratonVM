use super::{compiled_from_the_current_bytecode, resume_current_body_of_redefined_class};
use crate::classloading::ClassId;

fn body_stamped_now() -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
    buf.emit(&[0xC3]); // ret
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.install_epoch = cratonvm_jit::jit_install_epoch();
    cm
}

#[test]
fn only_a_body_compiled_after_the_redefinition_is_current() {
    if !resume_current_body_of_redefined_class() {
        return;
    }
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let redefined = ClassId::new(7);
    let other = ClassId::new(8);
    let before = body_stamped_now();
    assert!(compiled_from_the_current_bytecode(
        &shared, &before, redefined
    ));
    let _ = shared
        .jit
        .jit_cache
        .invalidate_for_redefinition(redefined, "p/Redefined");
    assert!(!compiled_from_the_current_bytecode(
        &shared, &before, redefined
    ));
    // Another class's redefinition barrier says nothing about this one.
    assert!(compiled_from_the_current_bytecode(&shared, &before, other));
    let after = body_stamped_now();
    assert!(compiled_from_the_current_bytecode(
        &shared, &after, redefined
    ));
}
