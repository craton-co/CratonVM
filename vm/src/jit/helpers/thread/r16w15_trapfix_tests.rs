use super::*;

fn vm() -> std::sync::Arc<SharedVm> {
    let config = crate::config::VmConfig {
        use_synthetic_jdk: true,
        ..Default::default()
    };
    let vm = std::sync::Arc::new(crate::vm::vm_init::SharedVm::new(config));
    *vm.self_arc.write() = Some(std::sync::Arc::downgrade(&vm));
    vm
}

/// `NOP5; MOV EAX, 7; RET`, a static `()I` body with the entry pad.
fn padded_body() -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&cratonvm_jit::ENTRY_PATCH_PAD);
    buf.emit(&[0xB8, 0x07, 0x00, 0x00, 0x00, 0xC3]);
    let mut body = cratonvm_jit::CompiledMethod::new(buf);
    body.entry_java_args = 0;
    body
}

const CLASS: &str = "r16w15/Trap";
const METHOD: &str = "kernel";
const DESC: &str = "()I";

fn publish(vm: &SharedVm, id: ClassId) -> std::sync::Arc<cratonvm_jit::CompiledMethod> {
    vm.jit
        .jit_cache
        .put(CLASS.into(), METHOD.into(), DESC.into(), id, padded_body());
    vm.jit
        .jit_cache
        .get(CLASS, METHOD, DESC, id)
        .expect("published")
}

/// A `MakeNotCompilable` eviction patches the body a running caller baked,
/// without the redefinition's mark; the service then leaves the next trap
/// of that method uncharged.
#[test]
fn a_not_compilable_eviction_patches_the_body_and_charges_once() {
    let vm = vm();
    let id = ClassId::new(0x1615);
    let body = publish(&vm, id);
    assert!(!trap_from_a_body_already_declared_not_compilable(
        &vm, CLASS, METHOD, DESC, id, 19
    ));
    // `UnreachedCode` is `MakeNotCompilable` on the first occurrence.
    let action = DeoptimizationController::deoptimize_in(
        &vm,
        id,
        CLASS,
        METHOD,
        DESC,
        cratonvm_jit::deopt::DeoptReason::UnreachedCode,
        19,
    );
    assert_eq!(action, cratonvm_jit::deopt::DeoptAction::MakeNotCompilable);
    assert!(vm.jit.jit_cache.get(CLASS, METHOD, DESC, id).is_none());
    assert!(body.is_not_entrant(), "the evicted entry is patched");
    assert!(!body.is_withdrawn_by_redefinition());
    assert_eq!(vm.jit.jit_cache.bodies_withdrawn_by_redefinition(), 0);
    assert!(trap_from_a_body_already_declared_not_compilable(
        &vm, CLASS, METHOD, DESC, id, 19
    ));
    // Another class id is another method.
    assert!(!trap_from_a_body_already_declared_not_compilable(
        &vm,
        CLASS,
        METHOD,
        DESC,
        ClassId::new(0x1616),
        19
    ));
    drop(cratonvm_jit::RetainedCode::new(body));
}

/// An eviction short of `MakeNotCompilable` (a first `BoundsCheck`,
/// `Reinterpret`, which still evicts) patches the body too -- a running
/// caller's `CALL` then forwards to the recompile -- and the service keeps
/// charging: no verdict exists yet.
#[test]
fn a_recompiling_eviction_patches_the_body_and_keeps_the_charge() {
    let vm = vm();
    let id = ClassId::new(0x1617);
    let body = publish(&vm, id);
    let reason = cratonvm_jit::deopt::DeoptReason::BoundsCheck;
    let action =
        DeoptimizationController::deoptimize_in(&vm, id, CLASS, METHOD, DESC, reason, 7);
    assert!(cratonvm_jit::tiered::deopt_evicts_method_body(
        reason, action
    ));
    assert_ne!(action, cratonvm_jit::deopt::DeoptAction::MakeNotCompilable);
    assert!(vm.jit.jit_cache.get(CLASS, METHOD, DESC, id).is_none());
    assert!(body.is_not_entrant());
    assert!(!trap_from_a_body_already_declared_not_compilable(
        &vm, CLASS, METHOD, DESC, id, 7
    ));
    drop(cratonvm_jit::RetainedCode::new(body));
}
