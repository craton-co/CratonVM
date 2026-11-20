use super::*;
use crate::config::VmConfig;
use crate::error::{LinkageError, MethodCallFailed, RuntimeError, VmError};
use crate::threading::jvm_thread::{JvmThread, ThreadId};

fn pending_class(
    shared: &SharedVm,
    thread: &mut JvmThread,
    err: MethodCallFailed,
    convert: bool,
) -> Option<String> {
    thread.jit_pending_exception = None;
    let stashed = stash_helper_failure(shared, thread, err, &"ctx", convert, convert);
    let pending = thread.jit_pending_exception.take();
    assert_eq!(stashed, pending.is_some());
    pending.and_then(|exc| throwable_class_name(shared, exc))
}

/// The class-init guards of `new` / `getstatic` / `putstatic` and the
/// monitor helpers used to wrap a Java-mappable failure (a `<clinit>`
/// entered at the depth ceiling, a linkage error) in `InternalError`.
#[test]
fn helper_failures_become_their_java_throwables() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "r12w4-exc2-helper-failure");
    if crate::runtime::exceptions::create_exception_object(
        &shared,
        &mut thread,
        "java/lang/InternalError",
        None,
    )
    .is_err()
    {
        return; // the unit VM cannot build a throwable at all
    }
    let cases: [(MethodCallFailed, &str); 4] = [
        (
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::StackOverflowError)),
            "java/lang/StackOverflowError",
        ),
        (
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::NotImplemented {
                feature: "operand stack overflow".to_string(),
            })),
            "java/lang/StackOverflowError",
        ),
        (
            MethodCallFailed::InternalError(VmError::Linkage(
                LinkageError::NoClassDefFoundError {
                    class_name: "r12w4/Missing".to_string(),
                },
            )),
            "java/lang/NoClassDefFoundError",
        ),
        (
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::NotImplemented {
                feature: "invokedynamic".to_string(),
            })),
            "java/lang/InternalError",
        ),
    ];
    for (err, want) in cases {
        // Skip a throwable class the unit VM cannot build at all.
        if crate::runtime::exceptions::create_exception_object(
            &shared,
            &mut thread,
            want,
            Some("probe"),
        )
        .is_err()
        {
            continue;
        }
        let got = pending_class(&shared, &mut thread, err, true);
        assert_eq!(got.as_deref(), Some(want));
    }
    // Conversion off: every internal error keeps the legacy wrap.
    let got = pending_class(
        &shared,
        &mut thread,
        MethodCallFailed::InternalError(VmError::Linkage(LinkageError::NoClassDefFoundError {
            class_name: "r12w4/Missing".to_string(),
        })),
        false,
    );
    assert_eq!(got.as_deref(), Some("java/lang/InternalError"));
}
