use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::cell::{Cell, RefCell};

thread_local! {
    static ITER_OLD: Cell<Option<ObjectRef>> = const { Cell::new(None) };
    static ITER_NEW: Cell<Option<ObjectRef>> = const { Cell::new(None) };
    static CALLS: Cell<usize> = const { Cell::new(0) };
    static NEXT_RECEIVERS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

fn relocating_iterator_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    match method {
        "iterator" => Some(Ok(Some(Value::Object(ITER_OLD.with(Cell::get))))),
        "hasNext" => Some(Ok(Some(Value::Int(1)))),
        "next" => {
            NEXT_RECEIVERS.with(|r| r.borrow_mut().push(receiver.as_ptr() as usize));
            let task = ctx.fresh_object_ref();
            Some(Ok(Some(Value::Object(Some(task)))))
        }
        "call" => {
            let call = CALLS.with(|c| {
                let n = c.get();
                c.set(n + 1);
                n
            });
            if call == 0 {
                // The first task "collects" and fails, so the loop goes on.
                let old = ITER_OLD.with(Cell::get).unwrap();
                let new = ITER_NEW.with(Cell::get).unwrap();
                ctx.remap_native_pin_addr_for_test(
                    old.as_ptr() as usize,
                    new.as_ptr() as usize,
                );
                Some(Err(RuntimeError::IllegalStateException {
                    message: "first task fails".to_string(),
                }
                .into()))
            } else {
                Some(Ok(Some(Value::Int(7))))
            }
        }
        _ => None,
    }
}

#[test]
fn invoke_any_re_reads_its_iterator_after_a_task_collects() {
    let mut registry = NativeMethodRegistry::new();
    register_executor_natives(&mut registry);
    let es = "java/util/concurrent/ExecutorService";
    let invoke_any = registry
        .find(
            es,
            "invokeAny",
            "(Ljava/util/Collection;)Ljava/lang/Object;",
        )
        .expect("ExecutorService.invokeAny should be registered");

    let mut ctx = MockNativeContext::new();
    // A synthetic (not real) executor, so the inline path runs.
    let es_cid = ctx.ensure_class_initialized(es).unwrap();
    let executor = ctx.alloc_object(es_cid, 1);
    let tasks = ctx.fresh_object_ref();
    let iter_old = ctx.fresh_object_ref();
    let iter_new = ctx.fresh_object_ref();
    ITER_OLD.with(|c| c.set(Some(iter_old)));
    ITER_NEW.with(|c| c.set(Some(iter_new)));
    CALLS.with(|c| c.set(0));
    NEXT_RECEIVERS.with(|r| r.borrow_mut().clear());
    ctx.set_invoke_virtual_hook(relocating_iterator_hook);

    let result = invoke_any(
        &mut ctx,
        &[Value::Object(Some(executor)), Value::Object(Some(tasks))],
    )
    .expect("the second task succeeds");
    assert_eq!(result, Some(Value::Int(7)));
    let receivers = NEXT_RECEIVERS.with(|r| r.borrow().clone());
    assert_eq!(
        receivers,
        vec![iter_old.as_ptr() as usize, iter_new.as_ptr() as usize],
        "the second `next` went to the pre-collection iterator"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "the iterator root leaked"
    );
}
