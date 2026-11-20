use super::*;
// gci i18/na (orchestrator, i17a test build E0599): `new_array` /
// `new_ref_array` are `NativeHeapAccess` methods, not `NativeContext`'s.
use cratonvm_native_api::NativeHeapAccess as _;

/// The heap-exhaustion unwind is caught and handed back; the permission is
/// granted inside the scope and restored after it, on both exits.
#[test]
fn catch_alloc_oom_catches_the_allocators_unwind() {
    assert!(!unwind_ok());
    let ok = catch_alloc_oom(|| {
        assert!(unwind_ok());
        5
    });
    assert!(matches!(ok, Ok(5)));
    assert!(!unwind_ok());
    let refused: Result<(), NativeAllocOom> = catch_alloc_oom(|| raise(12, "object"));
    let oom = refused.expect_err("the unwind must be caught");
    assert_eq!((oom.length, oom.what), (12, "object"));
    assert!(
        !unwind_ok(),
        "the depth must be restored on the unwind exit"
    );
}

/// Any other panic is not this channel's: it propagates unchanged.
#[test]
fn catch_alloc_oom_resumes_every_other_panic() {
    let outer = std::panic::catch_unwind(|| {
        let _: Result<(), NativeAllocOom> =
            catch_alloc_oom(|| panic!("w4c: not an allocation failure"));
    });
    let payload = outer.expect_err("a foreign panic must propagate");
    assert_eq!(
        payload.downcast_ref::<&str>().copied(),
        Some("w4c: not an allocation failure")
    );
    assert!(!unwind_ok());
}

/// gc-common w5-c: an interpreter `catch_unwind` that caught the
/// allocators' unwind reports it as the `OutOfMemoryError` it stands for
/// (HotSpot's text for a length refusal, the heap text otherwise), and
/// never claims any other payload.
#[test]
fn a_caught_allocation_unwind_becomes_an_oome_call_failure() {
    let payload = std::panic::catch_unwind(|| {
        raise(9, "object");
    })
    .expect_err("raise must unwind");
    assert!(is_native_alloc_oom_payload(&*payload));
    assert!(matches!(
        caught_oom_as_call_failure(payload),
        crate::error::MethodCallFailed::InternalError(crate::error::VmError::Runtime(
            crate::error::RuntimeError::OutOfMemoryError { message }
        )) if message.starts_with("Java heap space")
    ));

    let limit = std::panic::catch_unwind(|| {
        raise(i32::MAX as usize, "byte[]");
    })
    .expect_err("raise must unwind");
    assert!(matches!(
        caught_oom_as_call_failure(limit),
        crate::error::MethodCallFailed::InternalError(crate::error::VmError::Runtime(
            crate::error::RuntimeError::OutOfMemoryError { message }
        )) if message == crate::runtime::interpreter::ARRAY_SIZE_EXCEEDS_VM_LIMIT
    ));

    let other = std::panic::catch_unwind(|| {
        panic!("w5c: a real bug");
    })
    .expect_err("a panic must unwind");
    assert!(!is_native_alloc_oom_payload(&*other));
    let resumed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = caught_oom_as_call_failure(other);
    }))
    .expect_err("a foreign payload is resumed, not mapped");
    assert_eq!(
        resumed.downcast_ref::<&str>().copied(),
        Some("w5c: a real bug")
    );
}

/// A length past HotSpot's limit is refused with HotSpot's message; the
/// heap arm keeps the heap text.
#[test]
fn the_runtime_error_carries_the_java_message() {
    let limit = NativeAllocOom {
        length: i32::MAX as usize,
        what: "primitive array",
    };
    assert!(matches!(
        limit.into_runtime_error(),
        crate::error::RuntimeError::OutOfMemoryError { message }
            if message == crate::runtime::interpreter::ARRAY_SIZE_EXCEEDS_VM_LIMIT
    ));
    let heap = NativeAllocOom {
        length: 3,
        what: "object",
    };
    assert!(matches!(
        heap.into_runtime_error(),
        crate::error::RuntimeError::OutOfMemoryError { message }
            if message.starts_with("Java heap space")
    ));
}

fn gci_i17na_fake_ref(addr: usize) -> crate::types::ObjectRef {
    // SAFETY: never dereferenced: only pushed onto, and truncated off,
    // the pin list.
    unsafe { crate::types::ObjectRef::from_raw(addr as *mut u8) }
}

/// gci i17/na: the per-thread scope answers its closure's result, grants
/// the permission inside and restores it after, and keeps the pins of a
/// scope that did not refuse.
#[test]
fn gci_i17na_the_thread_scope_passes_its_result_and_restores_the_permission() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    let mut thread = JvmThread::new(ThreadId(0), "gci-i17na-ok");
    assert!(!unwind_ok());
    let got = catch_alloc_oom_on(&mut thread, |t| {
        t.native_pin_roots.push(gci_i17na_fake_ref(0x4000));
        unwind_ok()
    });
    assert!(matches!(got, Ok(true)), "the permission is granted inside");
    assert!(!unwind_ok(), "and restored after");
    assert_eq!(thread.native_pin_roots.len(), 1, "a success keeps its pins");
}

/// A refusal drops exactly the pins the scope pushed (never the caller's)
/// and restores the permission on the unwind exit.
#[test]
fn gci_i17na_a_refusal_drops_only_the_scopes_pins() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    let mut thread = JvmThread::new(ThreadId(0), "gci-i17na-refused");
    thread.native_pin_roots.push(gci_i17na_fake_ref(0x1000));
    let got: Result<(), NativeAllocOom> = catch_alloc_oom_on(&mut thread, |t| {
        t.native_pin_roots.push(gci_i17na_fake_ref(0x2000));
        t.native_pin_roots.push(gci_i17na_fake_ref(0x3000));
        raise(16, "reference array")
    });
    let oom = got.expect_err("the refusal is handed back");
    assert_eq!((oom.length, oom.what), (16, "reference array"));
    assert_eq!(thread.native_pin_roots.len(), 1, "the caller's pin stays");
    assert!(!unwind_ok());
}

/// Any other panic is not this channel's: it propagates unchanged.
#[test]
fn gci_i17na_the_thread_scope_resumes_every_other_panic() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    let mut thread = JvmThread::new(ThreadId(0), "gci-i17na-other");
    let outer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _: Result<(), NativeAllocOom> =
            catch_alloc_oom_on(&mut thread, |_| panic!("gci-i17na: a real bug"));
    }));
    let payload = outer.expect_err("a foreign panic must propagate");
    assert_eq!(
        payload.downcast_ref::<&str>().copied(),
        Some("gci-i17na: a real bug")
    );
    assert!(!unwind_ok());
}

/// The real allocators, on a `NativeContextImpl` built OUTSIDE
/// `safe_native_call_impl` (the shape of every audited site): inside the
/// scope a request the heap cannot hold, and one past HotSpot's array
/// limit, are refused through the unwind instead of taking the
/// infallible arm (which aborts the process on Generational).
#[test]
fn gci_i17na_a_context_outside_the_funnel_refuses_instead_of_aborting() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig {
        gc_algorithm: crate::config::GcAlgorithm::Generational,
        max_heap_size: 256 << 20,
        ..crate::config::VmConfig::default()
    });
    let mut thread = JvmThread::new(ThreadId(0), "gci-i17na-ctx");
    let pins = thread.native_pin_roots.len();
    // Larger than the whole 256 MB heap, inside the VM limit.
    let heap_refusal: Result<crate::types::ObjectRef, NativeAllocOom> =
        catch_alloc_oom_on(&mut thread, |t| {
            let mut ctx = crate::vm::NativeContextImpl {
                shared: &shared,
                thread: t,
            };
            ctx.new_array(crate::memory::heap::ArrayElementType::Byte, 512 << 20)
        });
    let oom = heap_refusal.expect_err("a heap that cannot hold it refuses");
    assert_eq!(oom.what, "primitive array");
    assert!(
        oom.java_message().starts_with("Java heap space"),
        "{}",
        oom.java_message()
    );
    let limit_refusal: Result<crate::types::ObjectRef, NativeAllocOom> =
        catch_alloc_oom_on(&mut thread, |t| {
            let mut ctx = crate::vm::NativeContextImpl {
                shared: &shared,
                thread: t,
            };
            ctx.new_ref_array(crate::classloading::ClassId::new(0), i32::MAX as usize)
        });
    let oom = limit_refusal.expect_err("past the VM limit refuses");
    assert_eq!(
        oom.java_message(),
        crate::runtime::interpreter::ARRAY_SIZE_EXCEEDS_VM_LIMIT
    );
    assert_eq!(thread.native_pin_roots.len(), pins);
    assert!(!unwind_ok());
}

/// Source pin: each audited `NativeContextImpl` built outside
/// `safe_native_call_impl` that reaches an allocator now runs inside
/// `catch_alloc_oom_on`, opened at most `REACH` bytes before its call.
#[test]
fn gci_i17na_every_audited_site_runs_under_the_thread_scope() {
    const REACH: usize = 400;
    let sites = [
        (
            "launcher_signals.rs",
            include_str!("../launcher_signals.rs"),
            "match host_signal_disposition(&ctx, signal)",
        ),
        (
            "interpreter/opcodes.rs",
            include_str!("../interpreter/opcodes.rs"),
            "jfr_vm_events::monitor_entered(",
        ),
        (
            "interpreter/lambda.rs",
            include_str!("../interpreter/lambda.rs"),
            "ctx.invoke_virtual(obj, method, desc, &[])",
        ),
        (
            "interpreter/invoke.rs",
            include_str!("../interpreter/invoke.rs"),
            "lang_invoke::varhandle_exact_call_site_refusal(",
        ),
        (
            "agent_loader.rs",
            include_str!("../agent_loader.rs"),
            "instrument::append_system_search(&mut ctx, &path)",
        ),
    ];
    for (name, raw, call) in sites {
        let src = raw.replace("\r\n", "\n");
        let at = src
            .find(call)
            .unwrap_or_else(|| panic!("{name}: the site moved"));
        assert_eq!(
            src.matches(call).count(),
            1,
            "{name}: one site per needle"
        );
        let mut lo = at.saturating_sub(REACH);
        while !src.is_char_boundary(lo) {
            lo += 1;
        }
        assert!(
            src[lo..at].contains("native_oom::catch_alloc_oom_on("),
            "{name}: the context is built outside the thread scope"
        );
    }
}

/// gci i18/na: a scope opened while the permission is already raised (a
/// class initialised, or an agent's JVMTI call made, from inside a native
/// callback) catches the refusal itself, drops only its own pins, and
/// hands the OUTER depth back unchanged.
#[test]
fn gci_i18na_a_nested_scope_catches_its_own_refusal_and_restores_the_outer_depth() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    let mut thread = JvmThread::new(ThreadId(0), "gci-i18na-nested");
    let outer = catch_alloc_oom(|| {
        let depth = UNWIND_OK.with(|c| c.get());
        thread.native_pin_roots.push(gci_i17na_fake_ref(0x5000));
        let inner: Result<(), NativeAllocOom> = catch_alloc_oom_on(&mut thread, |t| {
            assert_eq!(UNWIND_OK.with(|c| c.get()), depth + 1);
            t.native_pin_roots.push(gci_i17na_fake_ref(0x6000));
            raise(24, "object")
        });
        let oom = inner.expect_err("the inner scope catches it");
        assert_eq!((oom.length, oom.what), (24, "object"));
        assert_eq!(UNWIND_OK.with(|c| c.get()), depth, "outer depth restored");
        assert!(unwind_ok(), "the outer permission still holds");
        thread.native_pin_roots.len()
    });
    assert!(matches!(outer, Ok(1)), "the outer scope saw no unwind");
    assert!(!unwind_ok());
}

/// Source pin, gci i18/na: the contexts this wave closed (built outside
/// `safe_native_call_impl`, reaching an allocator) run inside
/// `catch_alloc_oom_on`, opened at most `REACH` bytes before the call.
#[test]
fn gci_i18na_every_closed_site_runs_under_the_thread_scope() {
    const REACH: usize = 480;
    let sites = [
        (
            "vm/vm_util.rs",
            include_str!("../../vm/vm_util.rs"),
            "define_or_check_class_path_package(&mut ctx, class_id)",
        ),
        (
            "vm/vm_util.rs",
            include_str!("../../vm/vm_util.rs"),
            "jfr_vm_events::class_loaded(&mut ctx, class_id, init_ns as i64)",
        ),
        (
            "vm/vm_util.rs",
            include_str!("../../vm/vm_util.rs"),
            "jfr_backend::event_class_initialized(&mut ctx, class_id)",
        ),
        (
            "vm/vm_launcher.rs",
            include_str!("../../vm/vm_launcher.rs"),
            "\"jdk/internal/ref/CleanerFactory\",",
        ),
        (
            "vm/vm_launcher.rs",
            include_str!("../../vm/vm_launcher.rs"),
            "\"checkAndLoadMain\",",
        ),
        (
            "native/jni.rs",
            concat!(include_str!("../../native/jni/mod.rs"), "\n", include_str!("../../native/jni/j_boolean.rs"), "\n", include_str!("../../native/jni/raw_locals_counted.rs"), "\n", include_str!("../../native/jni/truncate_local_frames.rs"), "\n", include_str!("../../native/jni/jni_empty_array_elements.rs"), "\n", include_str!("../../native/jni/lambda_proxy_jni_construct.rs")),
            "async_socket::drain_completions_pub(&mut ctx)",
        ),
        (
            "native/jni.rs",
            concat!(include_str!("../../native/jni/mod.rs"), "\n", include_str!("../../native/jni/j_boolean.rs"), "\n", include_str!("../../native/jni/raw_locals_counted.rs"), "\n", include_str!("../../native/jni/truncate_local_frames.rs"), "\n", include_str!("../../native/jni/jni_empty_array_elements.rs"), "\n", include_str!("../../native/jni/lambda_proxy_jni_construct.rs")),
            "job(shared, thread)",
        ),
        (
            "native/jni.rs",
            concat!(include_str!("../../native/jni/mod.rs"), "\n", include_str!("../../native/jni/j_boolean.rs"), "\n", include_str!("../../native/jni/raw_locals_counted.rs"), "\n", include_str!("../../native/jni/truncate_local_frames.rs"), "\n", include_str!("../../native/jni/jni_empty_array_elements.rs"), "\n", include_str!("../../native/jni/lambda_proxy_jni_construct.rs")),
            "phases_early::release_current_thread_locals(",
        ),
        (
            "jvmti/native_env.rs",
            concat!(include_str!("../../jvmti/native_env/mod.rs"), "\n", include_str!("../../jvmti/native_env/has_cap.rs"), "\n", include_str!("../../jvmti/native_env/jit_chain_len.rs"), "\n", include_str!("../../jvmti/native_env/read_capabilities.rs")),
            "redefine_classes_for_jvmti(&mut ctx, &replaced)",
        ),
        (
            "jvmti/native_env.rs",
            concat!(include_str!("../../jvmti/native_env/mod.rs"), "\n", include_str!("../../jvmti/native_env/has_cap.rs"), "\n", include_str!("../../jvmti/native_env/jit_chain_len.rs"), "\n", include_str!("../../jvmti/native_env/read_capabilities.rs")),
            "read(&mut ctx)",
        ),
    ];
    for (name, raw, call) in sites {
        let src = raw.replace("\r\n", "\n");
        let at = src
            .find(call)
            .unwrap_or_else(|| panic!("{name}: the site `{call}` moved"));
        assert_eq!(src.matches(call).count(), 1, "{name}: one site per needle");
        let mut lo = at.saturating_sub(REACH);
        while !src.is_char_boundary(lo) {
            lo += 1;
        }
        assert!(
            src[lo..at].contains("native_oom::catch_alloc_oom_on("),
            "{name}: `{call}` is built outside the thread scope"
        );
    }
}
