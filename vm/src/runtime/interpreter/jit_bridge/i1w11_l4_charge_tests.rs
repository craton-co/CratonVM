use super::charge_own_stashed_trap;
use crate::classloading::resolution::CachedBytecodeMethod;
use crate::config::VmConfig;
use crate::vm::SharedVm;
use std::sync::Arc;

const CLASS: &str = "cratonvm/test/W11Charge";

fn body() -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // ret
    cratonvm_jit::CompiledMethod::new(buf)
}

/// Interpreter round i1 wave 11 (L4): a trap the sink's own body fired is
/// charged to its method — the charge the `CRATONVM_IR_DEOPT_RESUME`
/// resume used to skip — and not charged once a newer body is published.
#[test]
fn a_trap_of_the_sinks_own_body_is_charged_unless_superseded() {
    let shared = SharedVm::new(VmConfig::default());
    let id0 = cratonvm_types::ClassId::new(0);
    let cached = Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: id0,
            class_name: Arc::from(CLASS),
            method_name: Arc::from("m"),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            code: Arc::from(&[0x00u8, 0xb1][..]), // nop; return
            exception_table: Arc::from(Vec::new().into_boxed_slice()),
            max_stack: 2,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
        },
    ));
    let rframe = cratonvm_jit::deopt::ReconstructedFrame {
        class_id: 0,
        method_key: format!("{CLASS}.m:()V"),
        bci: 0,
        ..Default::default()
    };
    let key = format!("{CLASS}.m:()V");
    let count = |shared: &SharedVm| shared.jit.deopt_log.lock().deopt_count_at_bci(&key, 0);

    let ran = body();
    let before = count(&shared);
    charge_own_stashed_trap(&shared, &ran, &cached, &rframe, None);
    assert_eq!(count(&shared), before + 1, "the own body's trap is charged");

    // A newer body is published under the key: `ran` is superseded.
    shared.jit.jit_cache.write().put(
        Arc::from(CLASS),
        Arc::from("m"),
        Arc::from("()V"),
        id0,
        body(),
    );
    let before = count(&shared);
    charge_own_stashed_trap(&shared, &ran, &cached, &rframe, None);
    assert_eq!(
        count(&shared),
        before,
        "a superseded body's trap must not evict the fresh one"
    );
}

/// Round nio21 wave 7 (lane L2,
/// `r21-charat-trap-despec-skips-the-rerun-sinks-20261008`): the re-run
/// arm records the per-bci de-spec the resuming arm records, once the
/// site has been charged `PER_BCI_DESPEC_LIMIT` times, and not before.
#[test]
fn a_rerun_arm_charge_despecs_the_site_after_the_per_bci_limit() {
    if !super::super::deopt_resume::rerun_sink_per_bci_despec_enabled() {
        return; // the kill switch is set in this environment
    }
    let shared = SharedVm::new(VmConfig::default());
    let id0 = cratonvm_types::ClassId::new(0);
    let cached = Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: id0,
            class_name: Arc::from(CLASS),
            method_name: Arc::from("d"),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            code: Arc::from(&[0x00u8, 0xb1][..]), // nop; return
            exception_table: Arc::from(Vec::new().into_boxed_slice()),
            max_stack: 2,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
        },
    ));
    let rframe = cratonvm_jit::deopt::ReconstructedFrame {
        class_id: 0,
        method_key: format!("{CLASS}.d:()V"),
        bci: 0,
        ..Default::default()
    };
    let key = format!("{CLASS}.d:()V");
    let ran = body();
    let limit = super::super::deopt_resume::PER_BCI_DESPEC_LIMIT;
    for _ in 1..limit {
        charge_own_stashed_trap(&shared, &ran, &cached, &rframe, None);
    }
    assert!(
        !shared.jit.despec_registry.contains(&key, 0),
        "below the limit the site keeps its speculation"
    );
    charge_own_stashed_trap(&shared, &ran, &cached, &rframe, None);
    assert!(
        shared.jit.despec_registry.contains(&key, 0),
        "the limit-th charge on the re-run arm withdraws the site"
    );
    // The superseded-guard sentinel is never de-spec'd.
    let sentinel = cratonvm_jit::deopt::ReconstructedFrame {
        class_id: 0,
        method_key: format!("{CLASS}.d:()V"),
        bci: u32::MAX,
        ..Default::default()
    };
    for _ in 0..limit {
        charge_own_stashed_trap(&shared, &ran, &cached, &sentinel, None);
    }
    assert!(!shared.jit.despec_registry.contains(&key, u32::MAX));
}
