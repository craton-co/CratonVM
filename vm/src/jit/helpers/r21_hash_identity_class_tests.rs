//! JIT round 21, lane hash (NATIVE21-1): the VM's answer to the
//! identity-hash class query (`class_hash_code_identity_verdict`).
use super::*;
use crate::config::VmConfig;

/// Class id 0 (the arm's own exact compare), an id the store does not
/// hold, and the lambda-proxy / sentinel band all answer `0`, through the
/// query word the compiler sends.
#[test]
fn unprovable_classes_answer_zero() {
    let shared = SharedVm::new(VmConfig::default());
    let ask = |cid: u32| {
        vh_const_well_known_answer(
            &shared,
            cratonvm_jit::ir_lower::vh_const_query(
                cratonvm_jit::ir_lower::WELL_KNOWN_VH_CONST_IDENTITY_HASH_TAG,
                cid,
                0,
            ),
        )
    };
    assert_eq!(ask(0), Some(0));
    assert_eq!(ask(61_999), Some(0));
    assert_eq!(ask(0x8000_0001), Some(0));
    assert_eq!(ask(u32::MAX - 2), Some(0));
}
