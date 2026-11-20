use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

/// Drops the test's VM rows even when an assertion fails.
struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_concurrent_tables(vm);
        }
    }
}

fn object(ctx: &mut MockNativeContext, fields: usize) -> ObjectRef {
    ctx.alloc_object(cratonvm_types::ClassId::new(0), fields)
}

fn mock_on(vm: usize) -> MockNativeContext {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    ctx
}

/// A parked producer holds the queue, its thread and its element as
/// global roots; the consumer receives the element through its root, and
/// every root is released once the rendezvous is done.
#[test]
fn w18g_sq_a_parked_producer_holds_roots_and_releases_them_all() {
    const VM: usize = 0x18C0_0A01;
    let _teardown = Teardown(&[VM]);
    let mut ctx = mock_on(VM);
    let queue = object(&mut ctx, 2);
    let item = object(&mut ctx, 1);
    let key = ctx.identity_hash_code(queue);
    let baseline = ctx.global_root_count();

    let SqProduced::Filed(root, ticket) =
        sq_produce(&mut ctx, queue, key, Value::Object(Some(item)), true)
    else {
        panic!("no consumer waits, so the producer is filed");
    };
    assert_eq!(
        ctx.global_root_count(),
        baseline + 3,
        "the queue, the parked thread and the element are rooted"
    );
    assert!(!sq_producer_taken(VM, key, root, ticket));

    let got = p58_sq_poll(&mut ctx, &[Value::Object(Some(queue))]).unwrap();
    assert_eq!(got, Some(Value::Object(Some(item))));
    assert!(
        sq_producer_taken(VM, key, root, ticket),
        "the woken producer sees it was taken"
    );
    assert_eq!(ctx.global_root_count(), baseline, "every root is released");
    assert!(
        SQ_TABLE.peek(VM, |t| t.is_empty()).unwrap_or(true),
        "the emptied row is dropped"
    );
}

/// A non-blocking `offer` fulfils a parked consumer through a root; the
/// woken consumer collects it and nothing stays rooted.
#[test]
fn w18g_sq_offer_fulfils_a_parked_consumer() {
    const VM: usize = 0x18C0_0A02;
    let _teardown = Teardown(&[VM]);
    let mut ctx = mock_on(VM);
    let queue = object(&mut ctx, 2);
    let item = object(&mut ctx, 1);
    let key = ctx.identity_hash_code(queue);
    let baseline = ctx.global_root_count();

    let SqConsumed::Filed(root, ticket) = sq_consume(&mut ctx, queue, key, true) else {
        panic!("no producer waits, so the consumer is filed");
    };
    assert_eq!(
        sq_consumer_collect(&mut ctx, key, root, ticket, false),
        None,
        "a spurious wake-up keeps waiting"
    );
    let offered = p58_sq_offer(
        &mut ctx,
        &[Value::Object(Some(queue)), Value::Object(Some(item))],
    )
    .unwrap();
    assert_eq!(offered, Some(Value::Int(1)));
    assert_eq!(
        sq_consumer_collect(&mut ctx, key, root, ticket, false),
        Some(Value::Object(Some(item)))
    );
    assert_eq!(ctx.global_root_count(), baseline);
    // Nobody waits now: a second offer is refused and roots nothing.
    let again = p58_sq_offer(
        &mut ctx,
        &[Value::Object(Some(queue)), Value::Object(Some(item))],
    )
    .unwrap();
    assert_eq!(again, Some(Value::Int(0)));
    assert_eq!(ctx.global_root_count(), baseline);
}

/// Another VM's queue with the same identity hash does not see the row.
#[test]
fn w18g_sq_rows_are_per_vm() {
    const VM_A: usize = 0x18C0_0A03;
    const VM_B: usize = 0x18C0_0B03;
    let _teardown = Teardown(&[VM_A, VM_B]);
    let mut a = mock_on(VM_A);
    let mut b = mock_on(VM_B);
    let queue_a = object(&mut a, 2);
    let item = object(&mut a, 1);
    let queue_b = object(&mut b, 2);
    let key = a.identity_hash_code(queue_a);

    assert!(matches!(
        sq_produce(&mut a, queue_a, key, Value::Object(Some(item)), true),
        SqProduced::Filed(..)
    ));
    assert!(
        matches!(sq_consume(&mut b, queue_b, key, false), SqConsumed::Nothing),
        "VM B's queue with VM A's hash must not receive VM A's element"
    );
    let got = p58_sq_poll(&mut a, &[Value::Object(Some(queue_a))]).unwrap();
    assert_eq!(got, Some(Value::Object(Some(item))));
}

/// Two live queues of one VM with the same identity hash keep separate
/// rows: the queue root tells them apart.
#[test]
fn w18g_sq_two_queues_with_one_hash_keep_separate_rows() {
    const VM: usize = 0x18C0_0A04;
    const KEY: i32 = 0x0018_C0A4;
    let _teardown = Teardown(&[VM]);
    let mut ctx = mock_on(VM);
    let q1 = object(&mut ctx, 2);
    let q2 = object(&mut ctx, 2);
    let item1 = object(&mut ctx, 1);
    let item2 = object(&mut ctx, 1);

    assert!(matches!(
        sq_produce(&mut ctx, q1, KEY, Value::Object(Some(item1)), true),
        SqProduced::Filed(..)
    ));
    assert!(
        matches!(sq_consume(&mut ctx, q2, KEY, false), SqConsumed::Nothing),
        "q2 must not take q1's element"
    );
    assert!(matches!(
        sq_produce(&mut ctx, q2, KEY, Value::Object(Some(item2)), true),
        SqProduced::Filed(..)
    ));
    assert_eq!(
        SQ_TABLE.peek(VM, |t| t.get(&KEY).map_or(0, |rows| rows.len())),
        Some(2)
    );
    assert!(matches!(
        sq_consume(&mut ctx, q2, KEY, false),
        SqConsumed::Took(Value::Object(Some(o))) if o == item2
    ));
    assert!(matches!(
        sq_consume(&mut ctx, q1, KEY, false),
        SqConsumed::Took(Value::Object(Some(o))) if o == item1
    ));
}

/// A timed `offer` that gives up withdraws its waiter and releases its
/// roots; `clear()` (a JDK no-op) leaves a parked producer alone.
#[test]
fn w18g_sq_withdraw_releases_and_clear_is_a_no_op() {
    const VM: usize = 0x18C0_0A05;
    let _teardown = Teardown(&[VM]);
    let mut ctx = mock_on(VM);
    let queue = object(&mut ctx, 2);
    let item = object(&mut ctx, 1);
    let key = ctx.identity_hash_code(queue);
    let baseline = ctx.global_root_count();

    let SqProduced::Filed(root, ticket) =
        sq_produce(&mut ctx, queue, key, Value::Object(Some(item)), true)
    else {
        panic!("filed");
    };
    p58_sq_clear(&mut ctx, &[Value::Object(Some(queue))]).unwrap();
    assert!(
        !sq_producer_taken(VM, key, root, ticket),
        "clear() drops no waiter"
    );
    assert!(
        !sq_withdraw_producer(&mut ctx, key, root, ticket),
        "not taken"
    );
    assert_eq!(ctx.global_root_count(), baseline);
    assert!(sq_producer_taken(VM, key, root, ticket), "withdrawn");
}

fn publisher_native(name: &str, desc: &str) -> cratonvm_native_api::NativeCallback {
    let mut r = NativeMethodRegistry::new();
    register_p69_submission_publisher(&mut r);
    r.find("java/util/concurrent/SubmissionPublisher", name, desc)
        .expect("registered by register_p69_submission_publisher")
}

/// A dead publisher's closed-exception row goes at the sweep, its root at
/// the VM's next `closeExceptionally`; a moved publisher keeps its row.
#[test]
fn w18g_sp_closed_exception_rows_follow_their_publisher() {
    const VM: usize = 0x18C0_0A06;
    const MOVED: usize = 0x7718_C0A0;
    let _teardown = Teardown(&[VM]);
    let mut ctx = mock_on(VM);
    let close = publisher_native("closeExceptionally", "(Ljava/lang/Throwable;)V");
    let get = publisher_native("getClosedException", "()Ljava/lang/Throwable;");
    let p1 = object(&mut ctx, 3);
    let p2 = object(&mut ctx, 3);
    let err1 = object(&mut ctx, 1);
    let err2 = object(&mut ctx, 1);
    let baseline = ctx.global_root_count();

    close(
        &mut ctx,
        &[Value::Object(Some(p1)), Value::Object(Some(err1))],
    )
    .unwrap();
    assert_eq!(
        get(&mut ctx, &[Value::Object(Some(p1))]).unwrap(),
        Some(Value::Object(Some(err1)))
    );
    assert_eq!(ctx.global_root_count(), baseline + 1);

    // A moving collection re-addresses the owner and keeps the row, even
    // under a verdict that calls every other address dead.
    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(p1.as_ptr() as usize, MOVED);
    assert_eq!(
        gc_sweep_submission_publisher_rows(VM, &moved, &|_| false),
        0
    );
    // The next collection finds the publisher dead at its new address.
    let nothing_moved = cratonvm_types::PointerMap::default();
    assert_eq!(
        gc_sweep_submission_publisher_rows(VM, &nothing_moved, &|a| a != MOVED),
        1
    );
    assert_eq!(
        get(&mut ctx, &[Value::Object(Some(p1))]).unwrap(),
        Some(Value::Object(None)),
        "the row went with its publisher"
    );
    assert_eq!(
        ctx.global_root_count(),
        baseline + 1,
        "queued, not yet released"
    );

    close(
        &mut ctx,
        &[Value::Object(Some(p2)), Value::Object(Some(err2))],
    )
    .unwrap();
    assert_eq!(
        ctx.global_root_count(),
        baseline + 1,
        "the next closeExceptionally released the swept root"
    );
    assert_eq!(
        get(&mut ctx, &[Value::Object(Some(p2))]).unwrap(),
        Some(Value::Object(Some(err2)))
    );
    // Another VM's sweep never judges this VM's rows.
    assert_eq!(
        gc_sweep_submission_publisher_rows(0x18C0_0B06, &nothing_moved, &|_| false),
        0
    );
    assert_eq!(
        get(&mut ctx, &[Value::Object(Some(p2))]).unwrap(),
        Some(Value::Object(Some(err2)))
    );
}

/// gc-common w29-a (`common-w28b-remaining-identity-hash-keyed-side-tables`
/// rank 23): two publishers of one VM with one identity hash keep their
/// own closed-exception rows, and one's death leaves the other's. The
/// mock's identity hash is the address truncated to `i32`, so two fake
/// publishers 4 GiB apart share one; the filing and lookup paths only hash
/// and compare them.
#[test]
fn w29a_colliding_publishers_keep_their_own_closed_exceptions() {
    const VM: usize = 0x29a0_0A06;
    let _teardown = Teardown(&[VM]);
    let ctx = mock_on(VM);
    // SAFETY: hashed and compared only, never dereferenced.
    let (p_a, p_b) = unsafe {
        (
            ObjectRef::from_raw(0x1_29a0_0a08usize as *mut u8),
            ObjectRef::from_raw(0x2_29a0_0a08usize as *mut u8),
        )
    };
    let key = ctx.identity_hash_code(p_a);
    assert_eq!(
        key,
        ctx.identity_hash_code(p_b),
        "premise: one identity hash"
    );
    let (addr_a, addr_b) = (p_a.as_ptr() as usize, p_b.as_ptr() as usize);
    // Stand-in root handles: the table only stores and hands them back.
    let (root_a, root_b) = (0x29a1, 0x29a2);
    assert_eq!(
        sp_file_closed_row(VM, key, addr_a, root_a),
        (None, Vec::new())
    );
    assert_eq!(
        sp_file_closed_row(VM, key, addr_b, root_b),
        (None, Vec::new()),
        "B's closeExceptionally replaced (and released) A's Throwable"
    );
    assert_eq!(sp_closed_exception_root(&ctx, p_a), Some(root_a));
    assert_eq!(
        sp_closed_exception_root(&ctx, p_b),
        Some(root_b),
        "B's getClosedException answered A's Throwable"
    );
    // A dies; B's row stays.
    let nothing_moved = cratonvm_types::PointerMap::default();
    assert_eq!(
        gc_sweep_submission_publisher_rows(VM, &nothing_moved, &|a| a != addr_a),
        1
    );
    assert_eq!(sp_closed_exception_root(&ctx, p_a), None);
    assert_eq!(sp_closed_exception_root(&ctx, p_b), Some(root_b));
    assert_eq!(
        sp_file_closed_row(VM, key, addr_b, root_b),
        (Some(root_b), vec![root_a]),
        "a re-file replaces only B's own row, and hands back A's swept root"
    );
}
