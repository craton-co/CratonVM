use super::*;

const K: usize = 1024;
const M: usize = 1024 * 1024;

fn b(extra: usize, java_stack: usize) -> SelfCallBudget {
    SelfCallBudget { extra, java_stack }
}

/// The compiled budget follows the configured Java stack size.
#[test]
fn r23_w6_the_budget_follows_the_java_stack_size() {
    assert_eq!(b(0, 0).bytes(), 16 * M, "default -Xss");
    assert_eq!(b(0, M).bytes(), 16 * M);
    assert_eq!(b(0, 256 * K).bytes(), 4 * M, "-Xss256k");
    assert_eq!(b(16 * M, 16 * M).bytes(), 256 * M, "-Xss16m");
}

/// The thread's guard floor still bounds a budget larger than its stack.
#[test]
fn r23_w6_the_guard_floor_still_bounds_the_budget() {
    let sp = 0x10_0000_0000usize;
    let roomy = Some((sp - 300 * M, sp + M));
    let tight = Some((sp - 8 * M, sp + M));
    assert_eq!(
        compute_self_call_stack_floor_in(sp, b(0, 0), roomy),
        sp - 16 * M
    );
    assert_eq!(
        compute_self_call_stack_floor_in(sp, b(0, 256 * K), roomy),
        sp - 4 * M
    );
    assert_eq!(
        compute_self_call_stack_floor_in(sp, b(0, 16 * M), roomy),
        sp - 256 * M
    );
    // A 9 MiB stack: the guard floor (1 MiB above its low end) binds.
    assert_eq!(
        compute_self_call_stack_floor_in(sp, b(0, 0), tight),
        sp - 8 * M + M
    );
}
