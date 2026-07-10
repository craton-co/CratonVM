from pathlib import Path

path = Path("/data/data/codex-wildfly-close-socket-inventory-20260708-202526/native-builtins/src/xnio_worker.rs")
text = path.read_text()

replacements = [
    (
        "use crate::{alloc_concurrent_synthetic, obj_arg};",
        "use crate::{alloc_concurrent_synthetic, obj_arg, spawn_runnable_on_real_thread};",
    ),
    (
        """fn native_worker_execute(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let worker =
        read_worker(ctx, this).ok_or_else(|| mcf_runtime("XnioWorker.execute: not registered"))?;
    // We can't capture the bytecode Runnable across threads in the
    // synthetic path (no JvmThread handle here).  Instead submit a
    // no-op and let the bytecode layer post-process the result.  The
    // Rust-facing `execute` API (tested) does the real work.
    let submitted = worker.execute(|| {});
    if submitted.is_err() {
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IllegalStateException {
                message: "XnioWorker.execute: worker is shutdown".to_string(),
            },
        )));
    }
    let _ = args.get(1);
    Ok(None)
}
""",
        """fn native_worker_execute(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let runnable = match args.get(1) {
        Some(Value::Object(Some(runnable))) => *runnable,
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("XnioWorker.execute: null Runnable".to_string()),
                },
            )));
        }
    };
    let worker =
        read_worker(ctx, this).ok_or_else(|| mcf_runtime("XnioWorker.execute: not registered"))?;
    if worker.is_shutdown() {
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IllegalStateException {
                message: "XnioWorker.execute: worker is shutdown".to_string(),
            },
        )));
    }
    spawn_runnable_on_real_thread(ctx, runnable)
}
""",
    ),
    (
        "    use std::sync::atomic::AtomicI32;",
        """    use std::sync::atomic::{AtomicI32, AtomicUsize};

    static NATIVE_EXECUTE_EXPECTED_RUNNABLE: AtomicUsize = AtomicUsize::new(0);
    static NATIVE_EXECUTE_SEEN_RUNNABLE: AtomicUsize = AtomicUsize::new(0);

    fn native_execute_runnable_hook(
        _ctx: &mut crate::test_utils::MockNativeContext,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        let expected = NATIVE_EXECUTE_EXPECTED_RUNNABLE.load(Ordering::SeqCst);
        if expected == 0 {
            return None;
        }
        let receiver_matches = receiver.as_ptr() as usize == expected;
        let arg_matches = matches!(
            args.first(),
            Some(Value::Object(Some(arg))) if arg.as_ptr() as usize == expected
        );
        if receiver_matches && method_name == "run" && descriptor == "()V" {
            NATIVE_EXECUTE_SEEN_RUNNABLE.fetch_add(1, Ordering::SeqCst);
            return Some(Ok(None));
        }
        if arg_matches && method_name == "execute" && descriptor == "(Ljava/lang/Runnable;)V" {
            NATIVE_EXECUTE_SEEN_RUNNABLE.fetch_add(1, Ordering::SeqCst);
            return Some(Ok(None));
        }
        None
    }""",
    ),
    (
        """        w.shutdown_now();
        assert!(w.await_termination(Duration::from_secs(30)));
    }

    #[test]
    fn t19_7_b_execute_after_shutdown_rejects_with_exception() {
""",
        """        w.shutdown_now();
        assert!(w.await_termination(Duration::from_secs(30)));
    }

    #[test]
    fn t19_7_b_native_execute_runs_java_runnable() {
        let w = XnioWorker::new(
            "t19_7_b_native_exec",
            OptionMap {
                worker_io_threads: Some(1),
                worker_task_core_threads: Some(1),
            },
        );
        let mut ctx = mock_ctx();
        let mirror = alloc_worker_mirror(&mut ctx, CLS_XNIO_WORKER, &w);
        register_worker(w.clone());
        let runnable = ctx.fresh_object_ref();
        NATIVE_EXECUTE_EXPECTED_RUNNABLE.store(runnable.as_ptr() as usize, Ordering::SeqCst);
        NATIVE_EXECUTE_SEEN_RUNNABLE.store(0, Ordering::SeqCst);
        ctx.set_invoke_virtual_hook(native_execute_runnable_hook);

        native_worker_execute(
            &mut ctx,
            &[Value::Object(Some(mirror)), Value::Object(Some(runnable))],
        )
        .expect("native execute should accept runnable");

        assert_eq!(NATIVE_EXECUTE_SEEN_RUNNABLE.load(Ordering::SeqCst), 1);
        NATIVE_EXECUTE_EXPECTED_RUNNABLE.store(0, Ordering::SeqCst);
        w.shutdown_now();
        assert!(w.await_termination(Duration::from_secs(2)));
    }

    #[test]
    fn t19_7_b_execute_after_shutdown_rejects_with_exception() {
""",
    ),
]

for old, new in replacements:
    if old not in text:
        raise SystemExit(f"missing replacement target:\n{old[:200]}")
    text = text.replace(old, new, 1)

path.write_text(text)
