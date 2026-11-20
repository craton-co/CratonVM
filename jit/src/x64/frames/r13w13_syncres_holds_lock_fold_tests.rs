//! Round 13 wave 13 (lane syncres, proposal M3-1 (a)): which
//! `invokestatic Thread.holdsLock` sites of a self-locking body fold to
//! `true`.

use super::super::emit::tests::test_compiler;

fn info(method_name: &'static str) -> *const crate::JitInvokeInfo {
    // LEAK(intentional): test-only; the compiler reads it through a raw
    // pointer, as it does the planner's rows.
    Box::leak(Box::new(crate::JitInvokeInfo {
        class_name: "java/lang/Thread",
        method_name,
        descriptor: "(Ljava/lang/Object;)Z",
        num_jit_args: 1,
        return_type: b'Z',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    })) as *const crate::JitInvokeInfo
}

/// An instance self-locking compile with `method_name`'s row at `pc`.
fn compiler_at(pc: usize, method_name: &'static str) -> super::super::Compiler {
    let mut c = test_compiler();
    c.self_lock_obj_off = 8;
    c.invoke_info.push((pc, info(method_name)));
    c.invoke_info_idx.insert(pc, 0);
    c
}

fn folds(c: &super::super::Compiler, code: &[u8], pc: usize) -> bool {
    let mut targets = vec![false; code.len() + 1];
    targets[0] = true;
    cratonvm_types::flags::with_thread_overrides(
        &[],
        || c.self_lock_holds_lock_fold_at(code, code.len(), pc, &targets),
    )
}

