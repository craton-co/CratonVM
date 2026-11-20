use super::*;
use crate::config::VmConfig;
use crate::error::{MethodCallFailed, RuntimeError, VmError};
use crate::threading::jvm_thread::{JvmThread, ThreadId};

#[test]
fn only_the_stack_overflow_shapes_are_recognised() {
    assert!(dispatch_error_is_stack_overflow(
        &RuntimeError::StackOverflowError
    ));
    assert!(dispatch_error_is_stack_overflow(
        &RuntimeError::NotImplemented {
            feature: "operand stack overflow".to_string(),
        }
    ));
    assert!(!dispatch_error_is_stack_overflow(
        &RuntimeError::NotImplemented {
            feature: "invokedynamic".to_string(),
        }
    ));
    assert!(!dispatch_error_is_stack_overflow(
        &RuntimeError::NegativeArraySizeException { size: -1 }
    ));
}

/// R12HuntSoeRecover phase 5: an overflow reported under a compiled
/// caller's dispatch must reach Java as `StackOverflowError`, not as an
/// `InternalError` wrapping its text.
#[test]
fn a_dispatch_stack_overflow_is_raised_as_stack_overflow_error() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "r12w3-hfix-soe");
    if crate::runtime::exceptions::create_exception_object(
        &shared,
        &mut thread,
        "java/lang/StackOverflowError",
        None,
    )
    .is_err()
    {
        return; // the unit VM cannot build the throwable at all
    }
    for err in [
        RuntimeError::StackOverflowError,
        RuntimeError::NotImplemented {
            feature: "operand stack overflow".to_string(),
        },
    ] {
        thread.jit_pending_exception = None;
        let got = handle_jit_dispatch_error(
            &shared,
            &mut thread,
            MethodCallFailed::InternalError(VmError::Runtime(err)),
            &INTEGER_VALUE_OF_INFO,
        );
        let pending = thread.jit_pending_exception.take();
        assert_eq!(got, i64::MIN);
        assert_eq!(
            pending
                .and_then(|exc| throwable_class_name(&shared, exc))
                .as_deref(),
            Some("java/lang/StackOverflowError")
        );
    }
}
