use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::ThreadId;
use std::sync::Arc;

const BODY_CLASS: u32 = 2306;
const LABEL: &str = "i23l6/Withdrawn.spin:()V";

#[test]
fn a_withdrawn_body_leaves_with_no_agent_listening() {
    std::thread::spawn(|| {
        let _lock = crate::runtime::jvmti::jvmti_registry_test_lock();
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        shared.classes.class_manager.write().register_class_name(
            cratonvm_types::ClassLoaderId::Application,
            "i23l6/Withdrawn",
            ClassId::new(BODY_CLASS),
        );
        let thread = JvmThread::new(ThreadId(0), "loop");
        let known = |withdrawn: bool| {
            PollingBody::from_artifact(BODY_CLASS, LABEL).map(|body| {
                body.with_artifact_facts((0x1000, 0x1040), 0)
                    .with_withdrawn_by_redefinition(withdrawn)
            })
        };
        assert!(known(true).is_some());
        assert_eq!(
            compiled_frame_exit_verdict(&shared, Some(&thread), known(false)),
            0,
            "no agent, not withdrawn: nothing"
        );
        // No exit point known: no sink could match its stash.
        let unknown = PollingBody::from_artifact(BODY_CLASS, LABEL)
            .map(|body| body.with_withdrawn_by_redefinition(true));
        assert_eq!(
            compiled_frame_exit_verdict(&shared, Some(&thread), unknown),
            0
        );
        let osr = cratonvm_jit::deopt::DeoptReason::OsrExit;
        assert!(!exit_left_for_the_interpreter(
            &shared,
            osr,
            ClassId::new(BODY_CLASS),
            "spin",
            "()V"
        ));
        let both = cratonvm_jit::SAFEPOINT_VERDICT_INNERMOST_FRAME
            | cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY;
        assert_eq!(
            compiled_frame_exit_verdict(&shared, Some(&thread), known(true)),
            both,
            "withdrawn: an OSR body tests the one bit, a method-entry body the other"
        );
        // The grant the verdict recorded makes the exit a mode exit for
        // the sinks: resumed, not charged.
        assert!(exit_left_for_the_interpreter(
            &shared,
            osr,
            ClassId::new(BODY_CLASS),
            "spin",
            "()V"
        ));
        assert!(!exit_left_for_the_interpreter(
            &shared,
            cratonvm_jit::deopt::DeoptReason::UncommonTrap,
            ClassId::new(BODY_CLASS),
            "spin",
            "()V"
        ));
    })
    .join()
    .expect("test thread");
}
