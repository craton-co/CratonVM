use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::io::Write as _;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;

// --- the netty selected-key fast path (F4) -----------------------------
//
// These exist because the engagement census said this path had NEVER RUN:
// a whole-suite CratonVM run reported `selected-keys fast=0 generic=0`,
// because the regression suite has no netty on its classpath and the JDK
// selector it does use is served by the on-demand `selectedKeys()` builder
// instead of the field. Shipping a fast path that nothing has ever
// executed is worse than shipping no fast path, so the mechanism is
// covered here even though the integration is not.
//
// The mock these run against had `resolve_field_index_by_class_id` stubbed
// to `None` until this commit. That is not a harmless default: with it,
// `sel_set_slots` refuses, `append_selected_keys_fast` returns false, and
// a test asserting "the keys arrived" passes by way of the GENERIC path
// while claiming to cover the fast one. Every test below therefore asserts
// which path ran, not merely that the outcome was right.

/// Build a mock netty `SelectedSelectionKeySet`: an object whose slot 0 is
/// the `keys` array and slot 1 the `size` cursor.
fn netty_key_set(
    ctx: &mut crate::test_support::MockNativeContext,
    capacity: usize,
    size: i32,
) -> ObjectRef {
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
    ctx.declare_field(NETTY_SELECTED_KEY_SET, "keys", 0);
    ctx.declare_field(NETTY_SELECTED_KEY_SET, "size", 1);
    let set = ctx.alloc_object_with_class(2, NETTY_SELECTED_KEY_SET);
    let arr = ctx.new_ref_array(ClassId::new(0), capacity);
    ctx.set_field(set, 0, Value::Object(Some(arr)));
    ctx.set_field(set, 1, Value::Int(size));
    set
}

/// The fast path appends at `size`, advances it, and enters NO Java.
#[test]
fn netty_selected_key_set_is_filled_without_entering_the_interpreter() {
    use cratonvm_native_api::{NativeHeapAccess, NativeInvokeAccess};
    let mut ctx = crate::test_support::MockNativeContext::new();
    let set = netty_key_set(&mut ctx, 8, 2);
    let k0 = ctx.alloc_object(1);
    let k1 = ctx.alloc_object(1);

    let took_fast = append_selected_keys_fast(&mut ctx, set, &[k0, k1]);
    assert!(took_fast, "the fast path refused a netty set with room");

    // Appended AT the cursor, not at 0 — a fast path that ignored `size`
    // would overwrite keys netty had not yet consumed.
    let Value::Object(Some(arr)) = ctx.get_field(set, 0) else {
        panic!("keys array vanished");
    };
    assert_eq!(ctx.get_array_element(arr, 2), Value::Object(Some(k0)));
    assert_eq!(ctx.get_array_element(arr, 3), Value::Object(Some(k1)));
    assert_eq!(ctx.get_field(set, 1), Value::Int(4), "size not advanced");
    // Slots before the cursor are untouched.
    assert_eq!(ctx.get_array_element(arr, 0), Value::Object(None));

    // The whole point: no `Set.add` was dispatched. A callee reached by
    // `invoke_virtual` from a native gets no call site, no counter, and can
    // never tier up — so "the keys arrived" is only half the assertion.
    assert!(
        ctx.recorded_calls().is_empty(),
        "fast path still entered the interpreter: {:?}",
        ctx.recorded_calls()
    );
}

/// NEGATIVE CONTROL — the assertion above must be able to fail.
///
/// Same call on a set of a DIFFERENT class has to refuse, because a
/// subclass may override `add` and only the generic dispatch honours an
/// override. Without this, a fast path that matched every class would pass
/// the test above just as happily.
#[test]
fn a_foreign_key_set_is_refused_by_the_fast_path() {
    use cratonvm_native_api::NativeHeapAccess;
    let mut ctx = crate::test_support::MockNativeContext::new();
    ctx.declare_field("java/util/HashSet", "keys", 0);
    ctx.declare_field("java/util/HashSet", "size", 1);
    let set = ctx.alloc_object_with_class(2, "java/util/HashSet");
    let arr = ctx.new_ref_array(ClassId::new(0), 8);
    ctx.set_field(set, 0, Value::Object(Some(arr)));
    ctx.set_field(set, 1, Value::Int(0));
    let k = ctx.alloc_object(1);

    assert!(
        !append_selected_keys_fast(&mut ctx, set, &[k]),
        "the fast path claimed a set whose class it does not know"
    );
    assert_eq!(ctx.get_field(set, 1), Value::Int(0), "size moved anyway");
}

/// A full array is refused rather than grown.
///
/// Netty's own `add` grows when `size == keys.length`; reproducing
/// `increaseCapacity` here would mean allocating a `SelectionKey[]` of the
/// right component type, which is its bytecode's job. The STRICT `<` also
/// preserves netty's post-`add` invariant that `size < keys.length`.
#[test]
fn a_full_key_set_is_refused_rather_than_grown() {
    use cratonvm_native_api::NativeHeapAccess;
    let mut ctx = crate::test_support::MockNativeContext::new();
    // Room for exactly one more, but appending it would leave
    // `size == keys.length`, which is the state netty grows out of.
    let set = netty_key_set(&mut ctx, 4, 3);
    let k = ctx.alloc_object(1);
    assert!(
        !append_selected_keys_fast(&mut ctx, set, &[k]),
        "fast path filled the array to its limit instead of refusing"
    );
    assert_eq!(ctx.get_field(set, 1), Value::Int(3), "size moved anyway");
}

/// The kill switch really removes the path.
#[test]
fn the_fast_path_is_absent_when_its_switch_is_off() {
    use cratonvm_native_api::NativeHeapAccess;
    // `sel_fast_keys_engaged` latches in a `OnceLock`, so this asserts the
    // GATE rather than flipping it mid-process: with the switch unset the
    // path engages, which is what the first test already proves. What is
    // checked here is that an UNDESCRIBED class — the state a real run is
    // in before any layout resolves — refuses cleanly instead of panicking
    // or half-writing.
    let mut ctx = crate::test_support::MockNativeContext::new();
    let set = ctx.alloc_object_with_class(2, NETTY_SELECTED_KEY_SET);
    let k = ctx.alloc_object(1);
    assert!(
        !append_selected_keys_fast(&mut ctx, set, &[k]),
        "fast path accepted a set whose fields do not resolve"
    );
}

/// Allocate a unique fake fd for test registration. These are NEVER
/// handed to the net.rs registry — they're purely selector-local ids.
fn fake_fd() -> i32 {
    static NEXT: AtomicI32 = AtomicI32::new(0x7000_0001);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

// --- selector-registry lock discipline ---------------------------------
//
// `ParameterizedSslHandlerTest` stalled for a week on a three-way deadlock
// in this registry; `SelectorsRead`'s doc comment carries the measured
// `gdb -p` dump. These two tests pin both halves of the rule that closed
// it. They are `#[test]`s rather than a comment because the failure mode
// is a process that STOPS: nothing throws, nothing fails, and a suite
// reading exit codes sees a timeout with no cause.

/// MUST NOT DEADLOCK — a nested read while a writer is queued.
///
/// `parking_lot::RwLock::read()` blocks whenever a writer is waiting, so
/// that writers cannot starve. A thread that already holds a read guard
/// and calls `read()` again therefore parks behind that writer, and the
/// writer is waiting for the guard that same parked thread still holds.
/// `selectors_read()` takes `read_recursive()` for the nested acquisition,
/// which never queues behind a writer.
///
/// The nested read runs on a SPAWNED thread with a bounded `recv_timeout`,
/// so a regression FAILS the suite instead of hanging it forever — which
/// is exactly what the defect did to the netty class.
#[test]
fn a_nested_selectors_read_does_not_deadlock_behind_a_queued_writer() {
    let _wall_clock = wall_clock_guard();
    use std::sync::mpsc;
    use std::time::Duration;

    // Hold the OUTER guard on a worker thread, ask a second thread for the
    // write lock, then take the NESTED guard on the SAME worker. The whole
    // sequence has to live on one thread because the depth counter — and
    // the deadlock — are per-thread.
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<usize>();
    let (release_tx, release_rx) = mpsc::channel::<()>();

    let worker = thread::spawn(move || {
        let outer = selectors_read();
        let outer_len = outer.len();
        ready_tx.send(()).unwrap();
        // Give the writer time to actually queue. If it has not queued yet
        // the test still passes — it just stops being a regression test
        // for this run, which is why the writer is joined below.
        release_rx.recv_timeout(Duration::from_secs(5)).ok();
        let inner = selectors_read();
        let n = inner.len() + outer_len;
        done_tx.send(n).unwrap();
        drop(inner);
        drop(outer);
    });

    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("outer guard taken");
    let writer = thread::spawn(|| {
        // `selector_open` is the registry's only writer.
        selector_open()
    });
    // The writer is now queued (or about to be); let the worker nest.
    thread::sleep(Duration::from_millis(50));
    release_tx.send(()).unwrap();

    // 20s is far beyond any legitimate wait for a `HashMap::len()` under a
    // lock; before the fix this never completes at all.
    let n = done_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("nested selectors_read() deadlocked behind the queued writer");
    assert!(n < usize::MAX, "sanity: got a length back ({n})");
    worker.join().unwrap();
    let id = writer.join().unwrap();
    assert!(id > 0, "the writer must have completed too, not starved");
}

/// The counter that decides `read()` vs `read_recursive()` — both
/// directions, so a tracker that only ever counted up (and so sent every
/// acquisition down the recursive path, reintroducing writer starvation)
/// would fail here.
#[test]
fn the_selectors_read_depth_tracks_nesting_in_both_directions() {
    assert_eq!(selectors_read_depth(), 0, "a fresh thread holds nothing");
    let a = selectors_read();
    assert_eq!(selectors_read_depth(), 1);
    {
        let b = selectors_read();
        assert_eq!(selectors_read_depth(), 2);
        drop(b);
    }
    assert_eq!(selectors_read_depth(), 1, "the nested guard released");
    drop(a);
    assert_eq!(selectors_read_depth(), 0, "the outer guard released");
}

/// `SelectionKey.cancel()` must not still hold the registry guard when it
/// calls `selector_cancel`, which takes the same lock.
///
/// This is the exact call pair the `gdb` dump caught (`sk_cancel_public`
/// 3491 holding, `selector_cancel` 1041 parked). `SelectorsRead` makes the
/// nesting survivable; this asserts the path does not nest at all, so the
/// hot path keeps the FAIR lock and writers keep their protection from
/// starvation.
#[test]
fn selector_cancel_runs_with_no_registry_guard_held() {
    let id = selector_open();
    let fd = fake_fd();
    selector_register(id, fd, OP_READ, None, 0x6c0c, None).unwrap();

    // Mirror `sk_cancel_public`'s search-then-cancel, and assert the
    // guard is gone before the call. `selector_cancel` itself asserts
    // nothing; the depth reading here is what pins the shape.
    let found = {
        let regs = selectors_read();
        assert_eq!(selectors_read_depth(), 1, "the search holds the guard");
        // Scoped to THIS selector: the registry is process-wide and the
        // test harness runs these in parallel.
        regs.get(&id).and_then(|sel| {
            let st = sel.lock();
            st.keys.keys().next().map(|k| (id, *k))
        })
    };
    assert_eq!(
        selectors_read_depth(),
        0,
        "the guard must be dropped before selector_cancel is called"
    );
    let (sel_id, target) = found.expect("the registration is findable");
    selector_cancel(sel_id, target);

    let regs = selectors_read();
    let st = regs.get(&sel_id).unwrap().lock();
    assert!(
        st.keys.get(&target).map(|k| k.cancelled).unwrap_or(false),
        "the key must actually be marked cancelled"
    );
    drop(st);
    drop(regs);
    selector_close(sel_id);
}

/// Serialises the tests that measure WALL CLOCK against each other and
/// against the one test that deliberately queues a WRITER on the
/// process-wide selector registry.
///
/// `a_nested_selectors_read_does_not_deadlock_behind_a_queued_writer` has
/// to queue a writer to reproduce the deadlock it guards, and while a
/// writer is queued every `selectors_read()` in the process waits — a
/// neighbour's `selector_select(id, 0)` included. Measured: adding that
/// test made `t19_7_a_select_now_non_blocking` (budget 50 ms) fail in the
/// full-suite run and pass in isolation, which is the classic shape of a
/// timing test that has acquired a neighbour.
///
/// This is not a workaround for the deadlock test: `selector_open` already
/// took the write lock before it existed, so these eight have always been
/// perturbing each other; the new test only made it visible.
fn wall_clock_guard() -> parking_lot::MutexGuard<'static, ()> {
    static WALL_CLOCK: OnceLock<Mutex<()>> = OnceLock::new();
    WALL_CLOCK.get_or_init(|| Mutex::new(())).lock()
}

/// Create a connected client/server TcpStream pair bound to 127.0.0.1:0.
/// Returns (client, server).
fn make_stream_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let (server, _) = listener.accept().unwrap();
    (client, server)
}

#[test]
fn t19_7_a_selector_open_returns_valid_selector() {
    let id = selector_open();
    assert!(id > 0);
    assert_eq!(selector_key_count(id), 0);
    selector_close(id);
}

/// Two registrations whose channel has no OS socket yet must occupy two
/// slots. They both answered `net_fd == -1`, shared one HashMap entry, and
/// the second `insert` silently replaced the first —
/// `NioEventLoopTest.testChannelsRegistered` saw `keys().size() == 1` for
/// two successfully-registered `NioServerSocketChannel`s (netty registers
/// before it binds). Guards `SelectorState::alloc_unresolved_fd`.
#[test]
fn unresolved_registrations_do_not_share_one_slot() {
    let id = selector_open();
    // -1 is exactly what `channel_net_fd` answers for an unbound channel.
    selector_register(id, -1, 0, None, 0x1111, None).unwrap();
    assert_eq!(selector_key_count(id), 1);
    selector_register(id, -1, 0, None, 0x2222, None).unwrap();
    assert_eq!(
        selector_key_count(id),
        2,
        "two unbound registrations collapsed into one slot"
    );
    selector_register(id, -1, 0, None, 0x3333, None).unwrap();
    assert_eq!(selector_key_count(id), 3);
    // A resolved registration still keys on its real fd, and does not
    // collide with the placeholder space.
    let fd = fake_fd();
    selector_register(id, fd, OP_READ, None, 0x4444, None).unwrap();
    assert_eq!(selector_key_count(id), 4);
    selector_close(id);
}

/// A re-register of the SAME key object must reuse its slot rather than
/// allocate a second placeholder: `NioIoHandler.rebuildSelector` re-registers
/// every key, so allocating per call would leak one entry per rebuild and
/// `numRegistered()` would then OVER-count.
#[test]
fn re_registering_the_same_unresolved_key_reuses_its_slot() {
    // A stand-in for the Java SelectionKey: `selector_register` only ever
    // compares `key_obj` for equality, never dereferences it.
    let key = fake_ref(0x5150);
    let id = selector_open();
    selector_register(id, -1, 0, Some(key), 0x5150, None).unwrap();
    assert_eq!(selector_key_count(id), 1);
    selector_register(id, -1, OP_READ, Some(key), 0x5150, None).unwrap();
    assert_eq!(
        selector_key_count(id),
        1,
        "re-register of the same key allocated a second placeholder slot"
    );
    selector_close(id);
}

#[test]
fn t19_7_a_selector_close_invalidates_keys() {
    let id = selector_open();
    let fd = fake_fd();
    // C27: signature is now (id, fd, ops, key_obj: Option<ObjectRef>,
    // key_hash: i32, kind). Tests use None + a sentinel hash.
    selector_register(id, fd, OP_READ, None, 0xdead_beef_u32 as i32, None).unwrap();
    assert_eq!(selector_key_count(id), 1);
    selector_close(id);
    assert_eq!(selector_key_count(id), 0);
    // Further select returns ClosedSelectorException.
    let r = selector_select(id, 0);
    assert!(r.is_err(), "select after close must fail");
}

#[test]
fn t19_7_a_channel_register_returns_selection_key_with_interest_ops() {
    let id = selector_open();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let fd = fake_fd();
    selector_register(
        id,
        fd,
        OP_ACCEPT,
        None,
        0xcafe_babe_u32 as i32,
        Some(SelectableKind::Listener(listener)),
    )
    .unwrap();
    assert_eq!(selector_key_count(id), 1);
    let regs = selectors_read();
    let st = regs.get(&id).unwrap().lock();
    let k = st.keys.get(&fd).unwrap();
    assert_eq!(k.interest_ops, OP_ACCEPT);
    assert_eq!(k.key_hash, 0xcafe_babe_u32 as i32);
    assert!(k.key_obj.is_none());
    drop(st);
    drop(regs);
    selector_close(id);
}

#[test]
fn t19_7_a_select_returns_zero_with_no_ready_channels() {
    let id = selector_open();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let fd = fake_fd();
    selector_register(
        id,
        fd,
        OP_ACCEPT,
        None,
        0,
        Some(SelectableKind::Listener(listener)),
    )
    .unwrap();
    let r = selector_select(id, 0).unwrap();
    assert_eq!(r, 0);
    selector_close(id);
}

#[test]
fn t19_7_a_select_now_non_blocking() {
    let _wall_clock = wall_clock_guard();
    let id = selector_open();
    let start = Instant::now();
    let r = selector_select(id, 0).unwrap();
    let elapsed = start.elapsed();
    assert_eq!(r, 0);
    assert!(elapsed < Duration::from_millis(50), "elapsed={elapsed:?}");
    selector_close(id);
}

#[test]
fn t19_7_a_select_with_timeout_respects_deadline() {
    let _wall_clock = wall_clock_guard();
    let id = selector_open();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let fd = fake_fd();
    selector_register(
        id,
        fd,
        OP_ACCEPT,
        None,
        0,
        Some(SelectableKind::Listener(listener)),
    )
    .unwrap();
    let start = Instant::now();
    let r = selector_select(id, 50).unwrap();
    let elapsed = start.elapsed();
    assert_eq!(r, 0);
    assert!(
        elapsed >= Duration::from_millis(40),
        "should block ~50ms, got {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "should not block >> 50ms, got {elapsed:?}"
    );
    selector_close(id);
}

/// REGRESSION: `interestOps()` with no select parked must not make the
/// NEXT `select(timeout)` return immediately.
///
/// The interest-change nudge writes a byte into the wakeup pipe so a
/// select ALREADY blocked in `epoll_wait` re-evaluates. The byte does not
/// expire, so writing it with nobody parked leaves it for the next select,
/// which returns at once having selected nothing. A Netty event loop sets
/// interest ops between selects — it hit that on every iteration, and after
/// 512 in a row Netty rebuilds the selector, re-registers every channel,
/// sets more interest ops, and the storm feeds itself (268 rebuilds in one
/// `ReactorClientHttpRequestFactoryBuilderTests` run, 2026-08-05).
///
/// The assertion is on the WALL CLOCK, not on the return value: a correct
/// select and the defective one both answer 0 ready keys, and only the
/// elapsed time tells them apart. That is also exactly the condition Netty
/// counts.
#[test]
fn set_interest_before_a_select_does_not_shorten_it() {
    let _wall_clock = wall_clock_guard();
    let id = selector_open();
    let (_client, server) = make_stream_pair();
    let fd = fake_fd();
    selector_register(
        id,
        fd,
        OP_READ,
        None,
        0,
        Some(SelectableKind::Stream(server)),
    )
    .unwrap();

    // Nothing is ever written to the pair, so OP_READ cannot fire and each
    // select must wait out its timeout — with or without this call.
    selector_set_interest(id, fd, OP_READ).unwrap();

    let start = Instant::now();
    let n = selector_select(id, 200).unwrap();
    let elapsed = start.elapsed();
    assert_eq!(n, 0, "no channel is readable");
    assert!(
        elapsed >= Duration::from_millis(150),
        "interestOps() before select must not shorten it: returned after \
         {elapsed:?} of a 200ms timeout. This is the 'Selector.select() \
         returned prematurely' storm."
    );
    selector_close(id);
}

/// The other half of the gate: a select that IS parked must still be woken
/// by an interest change.
///
/// This is what the nudge was added for on 2026-07-18 — Tomcat arms
/// OP_WRITE after a partial gathering write, and without a wakeup the last
/// HTTP/2 frame sat queued until shutdown and the peer saw a truncated
/// GOAWAY (`dohead-post-fix-sporadic-residuals-FIXED`). Gating the nudge on
/// `in_flight_selects` could have deleted that fix instead of the storm, so
/// the two tests are written together and neither is meaningful alone.
#[test]
fn set_interest_wakes_a_select_that_is_already_parked() {
    let _wall_clock = wall_clock_guard();
    let id = selector_open();
    let (_client, server) = make_stream_pair();
    let fd = fake_fd();
    selector_register(
        id,
        fd,
        OP_READ,
        None,
        0,
        Some(SelectableKind::Stream(server)),
    )
    .unwrap();

    let handle = thread::spawn(move || {
        // Long enough that the main thread is parked in the kernel wait.
        thread::sleep(Duration::from_millis(100));
        selector_set_interest(id, fd, OP_READ | OP_WRITE).unwrap();
    });

    let start = Instant::now();
    let _ = selector_select(id, 5000).unwrap();
    let elapsed = start.elapsed();
    handle.join().unwrap();

    assert!(
        elapsed < Duration::from_millis(2500),
        "an interest change must wake a PARKED select: waited {elapsed:?} of \
         a 5000ms timeout. Gating the nudge must not delete the Tomcat \
         OP_WRITE wakeup it exists for."
    );
    selector_close(id);
}

#[test]
fn t19_7_a_select_returns_ready_read_after_accept() {
    let id = selector_open();
    let (mut client, server) = make_stream_pair();
    let server_fd = fake_fd();
    selector_register(
        id,
        server_fd,
        OP_READ,
        None,
        0,
        Some(SelectableKind::Stream(server)),
    )
    .unwrap();

    client.write_all(b"x").unwrap();
    client.flush().unwrap();
    thread::sleep(Duration::from_millis(20));

    let n = selector_select(id, 500).unwrap();
    assert_eq!(n, 1);

    let regs = selectors_read();
    let st = regs.get(&id).unwrap().lock();
    let k = st.keys.get(&server_fd).unwrap();
    assert_eq!(k.ready_ops & OP_READ, OP_READ);
    drop(st);
    drop(regs);
    selector_close(id);
    drop(client);
}

#[test]
fn t19_7_a_wakeup_unblocks_concurrent_select() {
    let _wall_clock = wall_clock_guard();
    let id = selector_open();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let fd = fake_fd();
    selector_register(
        id,
        fd,
        OP_ACCEPT,
        None,
        0,
        Some(SelectableKind::Listener(listener)),
    )
    .unwrap();

    let handle = thread::spawn(move || {
        let start = Instant::now();
        let r = selector_select(id, 10_000).unwrap();
        (r, start.elapsed())
    });

    thread::sleep(Duration::from_millis(50));
    selector_wakeup(id).unwrap();

    let (r, elapsed) = handle.join().unwrap();
    assert_eq!(r, 0);
    assert!(
        elapsed < Duration::from_millis(2_000),
        "wakeup should unblock within 2s, got {elapsed:?}"
    );
    selector_close(id);
}

#[test]
fn t19_7_a_interest_ops_change_takes_effect_next_select() {
    let id = selector_open();
    let (mut client, server) = make_stream_pair();
    let server_fd = fake_fd();
    selector_register(
        id,
        server_fd,
        0,
        None,
        0,
        Some(SelectableKind::Stream(server)),
    )
    .unwrap();
    client.write_all(b"y").unwrap();
    thread::sleep(Duration::from_millis(20));
    let n = selector_select(id, 0).unwrap();
    assert_eq!(n, 0, "zero interest produces no ready keys");

    selector_set_interest(id, server_fd, OP_READ).unwrap();
    let n = selector_select(id, 500).unwrap();
    assert_eq!(n, 1);
    selector_close(id);
    drop(client);
}

#[test]
fn t19_7_a_key_cancel_removes_from_next_select() {
    let id = selector_open();
    let (mut client, server) = make_stream_pair();
    let server_fd = fake_fd();
    selector_register(
        id,
        server_fd,
        OP_READ,
        None,
        0,
        Some(SelectableKind::Stream(server)),
    )
    .unwrap();
    assert_eq!(selector_key_count(id), 1);
    selector_cancel(id, server_fd);
    client.write_all(b"z").unwrap();
    thread::sleep(Duration::from_millis(20));
    let _ = selector_select(id, 0).unwrap();
    assert_eq!(selector_key_count(id), 0, "cancelled key pruned");
    selector_close(id);
    drop(client);
}

#[test]
fn t19_7_a_negative_timeout_throws_illegal_argument() {
    let id = selector_open();
    let r = selector_select(id, -5);
    assert!(r.is_err(), "negative timeout must fail");
    match r {
        Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::IllegalArgumentException { message: _ },
        ))) => {}
        _ => panic!("expected IllegalArgumentException, got {r:?}"),
    }
    selector_close(id);
}

#[test]
fn t19_7_a_wakeup_before_select_returns_immediately() {
    let _wall_clock = wall_clock_guard();
    let id = selector_open();
    selector_wakeup(id).unwrap();
    let start = Instant::now();
    let r = selector_select(id, 1_000).unwrap();
    let elapsed = start.elapsed();
    assert_eq!(r, 0);
    assert!(
        elapsed < Duration::from_millis(500),
        "pre-wakeup should short-circuit, got {elapsed:?}"
    );
    selector_close(id);
}

/// The `i64::MAX` (indefinite) arm of the LOW-LEVEL `selector_select` is
/// short-circuited by a pre-existing wakeup.
///
/// Scope note: this test does NOT cover the `0 -> i64::MAX` translation in
/// `selector_select_native`, which is the actual BUGFIX [nio-selector]
/// change — it starts one level below it, at `selector_select(id,
/// i64::MAX)`, so deleting the translation leaves it green.
/// `select_zero_maps_to_the_indefinite_wait_not_a_poll` below is the test
/// for the translation itself.
///
/// (The historical comment here claimed an un-woken
/// `selector_select(id, i64::MAX)` "would block the test forever". That has
/// not been true since the self-healing `select_infinite_cap_ms()` cap
/// landed: an indefinite wait now returns 0 after `DEFAULT_SELECT_CAP_MS`.
/// The pre-armed wakeup is still the point of THIS test, which is that the
/// short-circuit fires before the syscall.)
#[test]
fn nio_selector_indefinite_block_path_honors_wakeup() {
    let _wall_clock = wall_clock_guard();
    let id = selector_open();
    selector_wakeup(id).unwrap();
    let start = Instant::now();
    let r = selector_select(id, i64::MAX).unwrap();
    let elapsed = start.elapsed();
    assert_eq!(r, 0);
    assert!(
        elapsed < Duration::from_millis(500),
        "indefinite block must short-circuit on a pending wakeup, got {elapsed:?}"
    );
    selector_close(id);
}

/// BUGFIX [nio-selector]: `Selector.select(0)` must BLOCK, not poll.
///
/// The JDK contract is that only `selectNow()` polls; `select(0)` blocks
/// until a channel is ready or `wakeup()` fires. `selector_select_native`
/// implements that with one line — `if timeout == 0 { i64::MAX }` — and
/// without it an idiomatic `while (running) selector.select(0);` event loop
/// spins at 100% CPU.
///
/// This test drives `selector_select_native` itself, so the translation is
/// on the call path. The only observable difference between the two
/// timeouts is TIME (both return 0 ready keys), so the signal is amplified
/// over `ITERS` calls and checked against a floor derived from the
/// production cap itself (`select_infinite_cap_ms()`), so retuning the cap
/// retunes the test.
///
/// The assertion is deliberately a LOWER bound and nothing else: a loaded
/// host can only push a blocking measurement further into the passing
/// region, never out of it. The genuine poll is timed too, but only to
/// print alongside the failure — a ratio between the two would be an upper
/// bound in disguise and would flake when the poll loop hits a scheduler
/// stall.
///
/// Mutation this catches: delete `let timeout = if timeout == 0 { i64::MAX
/// } else { timeout };` from `selector_select_native`. Each call then costs
/// a non-blocking poll (microseconds) instead of one capped block, and the
/// floors below are missed by more than an order of magnitude.
#[test]
fn select_zero_maps_to_the_indefinite_wait_not_a_poll() {
    let _wall_clock = wall_clock_guard();
    const ITERS: u32 = 8;

    let id = selector_open();
    let mut ctx = crate::test_support::MockNativeContext::new();
    // Legacy synthetic-layout selector object: `selector_id_from_obj` falls
    // back to the indexed slots when the object is not in `sel_obj_ids`.
    let obj = ctx.alloc_object(SI_OPEN_FLAG + 1);
    ctx.set_field(obj, SI_ID, Value::Int(id));
    ctx.set_field(obj, SI_OPEN_FLAG, Value::Int(1));

    // Baseline: the genuine non-blocking probe. This is the path
    // `selectNow0` takes, and it is what `select(0)` WRONGLY took before
    // the fix.
    let t0 = Instant::now();
    for _ in 0..ITERS {
        assert_eq!(
            selector_select(id, 0).unwrap(),
            0,
            "a poll of an empty selector reports no ready keys"
        );
    }
    let poll_total = t0.elapsed();

    // The public `select(0)` overload, through the native that owns the
    // translation.
    let t1 = Instant::now();
    for _ in 0..ITERS {
        match selector_select_native(&mut ctx, &[Value::Object(Some(obj)), Value::Long(0)]) {
            Ok(Some(Value::Int(0))) => {}
            other => panic!("select(0) on an empty selector must return 0, got {other:?}"),
        }
    }
    let blocking_total = t1.elapsed();

    selector_close(id);

    // Half the nominal blocked time, so scheduler jitter and an early
    // return on the last iteration cannot trip it.
    let cap_ms = select_infinite_cap_ms().max(0) as u64;
    let floor = Duration::from_millis(cap_ms * u64::from(ITERS) / 2);
    assert!(
        blocking_total >= floor,
        "select(0) must map to the indefinite wait (capped at {cap_ms} ms per \
         call), so {ITERS} calls must take at least {floor:?}; took \
         {blocking_total:?} against {poll_total:?} for the same number of \
         genuine polls. A poll-sized figure here means the `0 -> i64::MAX` \
         translation in selector_select_native is gone and every \
         `while (running) selector.select(0);` loop is spinning at 100% CPU."
    );
}

#[test]
fn t19_7_a_op_constants_match_jdk() {
    assert_eq!(OP_READ, 1);
    assert_eq!(OP_WRITE, 4);
    assert_eq!(OP_CONNECT, 8);
    assert_eq!(OP_ACCEPT, 16);
}

#[test]
fn t19_7_a_take_pending_accepted_returns_stream_after_select() {
    let id = selector_open();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let fd = fake_fd();
    selector_register(
        id,
        fd,
        OP_ACCEPT,
        None,
        0,
        Some(SelectableKind::Listener(listener)),
    )
    .unwrap();

    // Connect a client — select should mark it ready.
    let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    thread::sleep(Duration::from_millis(20));
    let n = selector_select(id, 500).unwrap();
    assert_eq!(n, 1);
    let accepted = take_pending_accepted(id, fd);
    assert!(accepted.is_some(), "accepted stream should be available");
    selector_close(id);
    drop(client);
}

#[test]
fn t19_7_a_dummy_handle_never_ready() {
    // A selector key with no live handle never reports ready.
    let id = selector_open();
    let fd = fake_fd();
    selector_register(id, fd, OP_READ | OP_WRITE | OP_ACCEPT, None, 0, None).unwrap();
    let n = selector_select(id, 0).unwrap();
    assert_eq!(n, 0, "dummy handle never reports ready");
    selector_close(id);
}

#[test]
fn wp3_1_kernel_select_handles_many_concurrent_streams() {
    // Sanity check: the kernel-backed selector can multiplex N streams
    // and waking one of them unblocks select with the right key.
    let id = selector_open();
    let mut clients = Vec::new();
    let mut server_fds = Vec::new();
    for _ in 0..8 {
        let (client, server) = make_stream_pair();
        let fd = fake_fd();
        server_fds.push(fd);
        clients.push(client);
        selector_register(
            id,
            fd,
            OP_READ,
            None,
            0,
            Some(SelectableKind::Stream(server)),
        )
        .unwrap();
    }
    // Write to one client.
    clients[3].write_all(b"hello").unwrap();
    clients[3].flush().unwrap();
    thread::sleep(Duration::from_millis(20));
    let n = selector_select(id, 1_000).unwrap();
    assert!(n >= 1, "expected at least one ready key, got {n}");
    selector_close(id);
}

#[test]
fn wp3_1_register_real_entry_point_compiles() {
    // Confirm the public registration surface exists.
    let mut reg = cratonvm_native_api::NativeMethodRegistry::new();
    register_nio_selector_real(&mut reg);
    // No assertion on internal counts — just that it runs without panic.
}

/// Build a fake 8-byte-aligned `ObjectRef` from a small ordinal. Never
/// dereferenced — `gc_scan_selector_roots` only compares pointer
/// identity, so a synthetic non-heap pointer is sufficient here.
fn fake_ref(ordinal: usize) -> ObjectRef {
    // 8-byte aligned, non-null, well clear of the low page.
    let ptr = ((ordinal + 1) * 8 + 0x1_0000) as *mut u8;
    // SAFETY: non-null and 8-byte aligned; only used for identity, the
    // pointer is never read through.
    unsafe { ObjectRef::from_raw(ptr) }
}

#[test]
fn gc_scan_selector_roots_collects_sk_table_and_key_obj_refs() {
    // Seed a `SkState` carrying channel + selector + attachment, plus a
    // per-selector `key_obj`, then prove the root scan reports every one
    // — i.e. exactly the refs `sk_table_update_after_gc` remaps.
    let channel = fake_ref(0x5100);
    let selector = fake_ref(0x5200);
    let attachment = fake_ref(0x5300);
    let key_obj = fake_ref(0x5400);

    // Unique key so the row is isolated from any other test's entries in
    // the process-wide table.
    let sk_key = 0x7E57_0001_u32 as i32;
    {
        let mut table = sk_table().write();
        table.entry(sk_key).or_default().push(SkState {
            key: fake_ref(0x5500),
            channel,
            selector,
            interest_ops: OP_READ,
            ready_ops: 0,
            attachment: Some(attachment),
            cancelled: false,
            vm: TEST_SELECTOR_VM,
            self_describing: false,
        });
    }

    // A selector whose key carries a non-null `key_obj`.
    let id = selector_open();
    let fd = fake_fd();
    selector_register(id, fd, OP_READ, Some(key_obj), 0x7E57_0002_u32 as i32, None).unwrap();

    let mut roots: Vec<ObjectRef> = Vec::new();
    gc_scan_selector_roots(TEST_SELECTOR_VM, &mut roots);

    let contains = |r: ObjectRef| roots.iter().any(|x| x.as_ptr() == r.as_ptr());
    assert!(contains(channel), "channel must be rooted");
    assert!(contains(selector), "selector must be rooted");
    assert!(contains(attachment), "attachment must be rooted");
    assert!(contains(key_obj), "per-selector key_obj must be rooted");

    // No null roots may ever be injected.
    assert!(
        roots.iter().all(|r| !r.as_ptr().is_null()),
        "scan must never push a null root"
    );

    // Cleanup the process-wide seed so sibling tests are unaffected.
    sk_table().write().remove(&sk_key);
    selector_close(id);
}

#[test]
fn gc_scan_selector_roots_skips_absent_attachment() {
    // An entry with no attachment contributes only channel + selector.
    let channel = fake_ref(0x6100);
    let selector = fake_ref(0x6200);
    let sk_key = 0x7E57_0003_u32 as i32;
    {
        let mut table = sk_table().write();
        table.entry(sk_key).or_default().push(SkState {
            key: fake_ref(0x6500),
            channel,
            selector,
            interest_ops: 0,
            ready_ops: 0,
            attachment: None,
            cancelled: false,
            vm: TEST_SELECTOR_VM,
            self_describing: false,
        });
    }

    let mut roots: Vec<ObjectRef> = Vec::new();
    gc_scan_selector_roots(TEST_SELECTOR_VM, &mut roots);

    let contains = |r: ObjectRef| roots.iter().any(|x| x.as_ptr() == r.as_ptr());
    assert!(contains(channel), "channel must be rooted");
    assert!(contains(selector), "selector must be rooted");

    sk_table().write().remove(&sk_key);
}

/// gc-common w10-b (`handoff-w9b-nio-row-per-vm`): every selector-side
/// table is walked per VM. One row of each kind is stamped with VM A and
/// one with VM B, at fake addresses:
///
/// * the scan for A reports only A's refs;
/// * B's remap, with a map naming A's addresses, leaves A's rows alone,
///   and A's remap rewrites them;
/// * the forget drops only A's rows (and A's registry slot).
#[test]
fn w10b_selector_rows_are_scanned_remapped_and_forgotten_per_vm() {
    // Identities no mock (small counter) and no other test uses.
    let vm_a: usize = 0xB10A_0001;
    let vm_b: usize = 0xB10A_0002;
    let key_a = fake_ref(0xB100);
    let chan_a = fake_ref(0xB101);
    let sel_a = fake_ref(0xB102);
    let att_a = fake_ref(0xB103);
    let reg_key_a = fake_ref(0xB104);
    let key_b = fake_ref(0xB200);
    let chan_b = fake_ref(0xB201);
    let sel_b = fake_ref(0xB202);
    let reg_key_b = fake_ref(0xB204);
    // Distinct buckets for A and B, plus one bucket holding both (the
    // identity-hash collision case the `Vec` bucket exists for).
    let (h_a, h_b, h_shared) = (
        0x7E57_B101_u32 as i32,
        0x7E57_B102_u32 as i32,
        0x7E57_B103_u32 as i32,
    );
    let row = |key: ObjectRef,
               channel: ObjectRef,
               selector: ObjectRef,
               attachment: Option<ObjectRef>,
               vm: usize| SkState {
        key,
        channel,
        selector,
        interest_ops: OP_READ,
        ready_ops: 0,
        attachment,
        cancelled: false,
        vm,
        self_describing: false,
    };
    {
        let mut t = sk_table().write();
        t.entry(h_a)
            .or_default()
            .push(row(key_a, chan_a, sel_a, Some(att_a), vm_a));
        t.entry(h_b)
            .or_default()
            .push(row(key_b, chan_b, sel_b, None, vm_b));
        let shared = t.entry(h_shared).or_default();
        shared.push(row(fake_ref(0xB105), chan_a, sel_a, None, vm_a));
        shared.push(row(fake_ref(0xB205), chan_b, sel_b, None, vm_b));
    }
    let id_a = selector_open_in(vm_a);
    let id_b = selector_open_in(vm_b);
    selector_register(id_a, fake_fd(), OP_READ, Some(reg_key_a), h_a, None).unwrap();
    selector_register(id_b, fake_fd(), OP_READ, Some(reg_key_b), h_b, None).unwrap();
    {
        let mut ids = sel_obj_ids().write();
        ids.entry(h_a).or_default().push(SelectorObjId {
            object: sel_a,
            id: id_a,
            vm: vm_a,
        });
        ids.entry(h_b).or_default().push(SelectorObjId {
            object: sel_b,
            id: id_b,
            vm: vm_b,
        });
    }

    let has =
        |roots: &[ObjectRef], r: ObjectRef| roots.iter().any(|x| x.as_ptr() == r.as_ptr());

    // 1. The scan for A carries A's refs and none of B's.
    let mut roots_a = Vec::new();
    gc_scan_selector_roots(vm_a, &mut roots_a);
    for r in [key_a, chan_a, sel_a, att_a, reg_key_a] {
        assert!(has(&roots_a, r), "VM A's scan must root its own ref {r:?}");
    }
    for r in [key_b, chan_b, sel_b, reg_key_b] {
        assert!(
            !has(&roots_a, r),
            "VM A's scan must not root VM B's ref {r:?}"
        );
    }
    let mut roots_b = Vec::new();
    gc_scan_selector_roots(vm_b, &mut roots_b);
    assert!(has(&roots_b, reg_key_b) && has(&roots_b, key_b));
    assert!(!has(&roots_b, reg_key_a) && !has(&roots_b, key_a));

    // 2. A map naming A's addresses: applied for B it must not touch A's
    //    rows; applied for A it moves them.
    let moved = |r: ObjectRef| fake_ref(0xC000 + (r.as_ptr() as usize >> 3));
    let mut map = cratonvm_types::PointerMap::default();
    for r in [key_a, chan_a, sel_a, att_a, reg_key_a] {
        map.insert(r.as_ptr() as usize, moved(r).as_ptr() as usize);
    }
    sk_table_update_after_gc(vm_b, &map);
    {
        let t = sk_table().read();
        let a = &t[&h_a][0];
        assert_eq!(a.key, key_a, "VM B's remap rewrote VM A's key");
        assert_eq!(a.channel, chan_a);
        assert_eq!(a.attachment, Some(att_a));
    }
    assert_eq!(sel_obj_ids().read()[&h_a][0].object, sel_a);
    assert!(
        slot_of_key_obj(reg_key_a).is_some(),
        "VM B's remap rewrote VM A's registry key_obj"
    );
    sk_table_update_after_gc(vm_a, &map);
    {
        let t = sk_table().read();
        let a = &t[&h_a][0];
        assert_eq!(a.key, moved(key_a), "VM A's remap must rewrite its own row");
        assert_eq!(a.channel, moved(chan_a));
        assert_eq!(a.selector, moved(sel_a));
        assert_eq!(a.attachment, Some(moved(att_a)));
        assert_eq!(t[&h_shared][0].channel, moved(chan_a));
        assert_eq!(
            t[&h_shared][1].channel, chan_b,
            "B's row in a shared bucket moved"
        );
        assert_eq!(t[&h_b][0].key, key_b);
    }
    assert_eq!(sel_obj_ids().read()[&h_a][0].object, moved(sel_a));
    assert_eq!(
        slot_of_key_obj(moved(reg_key_a)).map(|(id, _)| id),
        Some(id_a)
    );

    // 3. Forgetting A drops A's rows and slot only.
    forget_vm_nio_rows(vm_a);
    {
        let t = sk_table().read();
        assert!(!t.contains_key(&h_a), "VM A's bucket must be gone");
        assert_eq!(
            t[&h_shared].len(),
            1,
            "only B's row survives the shared bucket"
        );
        assert_eq!(t[&h_shared][0].vm, vm_b);
        assert_eq!(t[&h_b].len(), 1);
    }
    {
        let ids = sel_obj_ids().read();
        assert!(!ids.contains_key(&h_a));
        assert_eq!(ids[&h_b].len(), 1);
    }
    assert!(
        selectors_read().get(&id_a).is_none(),
        "VM A's selector slot must be gone"
    );
    assert!(selectors_read().get(&id_b).is_some_and(|s| s.is_open()));
    let mut roots_b = Vec::new();
    gc_scan_selector_roots(vm_b, &mut roots_b);
    assert!(has(&roots_b, reg_key_b) && has(&roots_b, key_b) && has(&roots_b, sel_b));
    // Idempotent.
    forget_vm_nio_rows(vm_a);

    forget_vm_nio_rows(vm_b);
    assert!(!sk_table().read().contains_key(&h_shared));
    assert!(selectors_read().get(&id_b).is_none());
}

/// A closed selector holds no keys, so the GC hooks and the per-key walks
/// skip it without taking its lock (gc-common w10-b). Pins the invariant
/// the skip relies on: `selector_close` empties `keys`, and a closed slot
/// then contributes nothing to the scan.
#[test]
fn w10b_closed_selector_contributes_no_roots() {
    let vm: usize = 0xB10A_0003;
    let key = fake_ref(0xB300);
    let id = selector_open_in(vm);
    selector_register(
        id,
        fake_fd(),
        OP_READ,
        Some(key),
        0x7E57_B301_u32 as i32,
        None,
    )
    .unwrap();
    let mut roots = Vec::new();
    gc_scan_selector_roots(vm, &mut roots);
    assert!(roots.iter().any(|r| *r == key));
    selector_close(id);
    assert!(selectors_read()
        .get(&id)
        .is_some_and(|s| !s.is_open() && s.lock().keys.is_empty()));
    let mut roots = Vec::new();
    gc_scan_selector_roots(vm, &mut roots);
    assert!(
        roots.is_empty(),
        "a closed selector must contribute no roots"
    );
    assert_eq!(slot_of_key_obj(key), None);
    forget_vm_nio_rows(vm);
    assert!(selectors_read().get(&id).is_none());
}

// --- gc-common w11-b: selection-key rows are retired ---------------------
//
// `common-w10b-nio-selection-key-rows-are-never-removed`: a key's
// `sk_table` row -- a GC root for its key, channel, selector and
// attachment -- used to live for the life of the VM. These drive the real
// natives on a mock VM whose `SelectionKeyImpl` declares the JDK fields a
// retired key answers from, and check both halves of the fix: the row is
// gone once the key leaves the selector, and the key still answers.

const W11B_KEY_CLASS: &str = "sun/nio/ch/SelectionKeyImpl";

/// A mock VM at its own address `base` (the process-wide rows are matched
/// on addresses, and tests run in parallel), a selector object bound to a
/// fresh native selector of that VM, and a channel object. With
/// `self_describing`, the key class declares `channel`, `selector`,
/// `attachment` and `invalid` (jdk-25's names).
fn w11b_fixture(
    base: usize,
    self_describing: bool,
) -> (
    crate::test_support::MockNativeContext,
    ObjectRef,
    i32,
    ObjectRef,
) {
    let mut ctx = crate::test_support::MockNativeContext::with_address_base(base);
    if self_describing {
        ctx.declare_field(W11B_KEY_CLASS, "channel", 0);
        ctx.declare_field(W11B_KEY_CLASS, "selector", 1);
        ctx.declare_field(W11B_KEY_CLASS, "attachment", 2);
        ctx.declare_field(W11B_KEY_CLASS, "invalid", 3);
    }
    let vm = ctx.vm_identity();
    let sel_obj = ctx.alloc_object(0);
    let id = selector_open_in(vm);
    sel_obj_ids()
        .write()
        .entry(ctx.identity_hash_code(sel_obj))
        .or_default()
        .push(SelectorObjId {
            object: sel_obj,
            id,
            vm,
        });
    let channel = ctx.alloc_object(0);
    (ctx, sel_obj, id, channel)
}

/// `channel.register(selector, OP_READ, attachment)` through the native.
fn w11b_register(
    ctx: &mut crate::test_support::MockNativeContext,
    sel_obj: ObjectRef,
    channel: ObjectRef,
    attachment: Option<ObjectRef>,
) -> ObjectRef {
    match channel_register_native(
        ctx,
        &[
            Value::Object(Some(channel)),
            Value::Object(Some(sel_obj)),
            Value::Int(OP_READ),
            Value::Object(attachment),
        ],
    ) {
        Ok(Some(Value::Object(Some(key)))) => key,
        other => panic!("register answered {other:?}"),
    }
}

fn w11b_answer(
    ctx: &mut crate::test_support::MockNativeContext,
    native: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
    key: ObjectRef,
) -> Value {
    native(ctx, &[Value::Object(Some(key))])
        .expect("accessor")
        .expect("accessor answers a value")
}

/// The JDK lifecycle: `cancel()`, then the next select deregisters the
/// key. After it the row is gone and nothing of the key is a root any
/// more, while `channel()`, `selector()`, `attachment()` and `isValid()`
/// answer exactly as they did right after the cancel.
#[test]
fn w11b_cancelled_key_row_is_retired_by_the_next_select() {
    let (mut ctx, sel_obj, id, channel) = w11b_fixture(0x5B11_0000, true);
    let vm = ctx.vm_identity();
    let att = ctx.alloc_object(0);
    assert_eq!(sk_row_count(vm), 0);
    let key = w11b_register(&mut ctx, sel_obj, channel, Some(att));
    assert_eq!(sk_row_count(vm), 1);
    // Registration wrote the key's own answers.
    assert_eq!(ctx.get_field(key, 0), Value::Object(Some(channel)));
    assert_eq!(ctx.get_field(key, 1), Value::Object(Some(sel_obj)));
    assert_eq!(ctx.get_field(key, 2), Value::Object(Some(att)));
    assert_eq!(w11b_answer(&mut ctx, sk_is_valid, key), Value::Int(1));

    key_cancel_native(&mut ctx, &[Value::Object(Some(key))]).unwrap();
    assert_eq!(
        ctx.get_field(key, 3),
        Value::Int(1),
        "cancel sets `invalid`"
    );
    assert_eq!(w11b_answer(&mut ctx, sk_is_valid, key), Value::Int(0));
    assert_eq!(sk_row_count(vm), 1, "the row lives until the next select");

    selector_select_now_native(&mut ctx, &[Value::Object(Some(sel_obj))]).unwrap();
    assert_eq!(sk_row_count(vm), 0, "the select retired the row");
    assert_eq!(selector_key_count(id), 0);
    assert!(selectors_read()
        .get(&id)
        .is_some_and(|s| s.lock().pending_retire.is_empty()));

    assert_eq!(
        w11b_answer(&mut ctx, sk_channel, key),
        Value::Object(Some(channel))
    );
    assert_eq!(
        w11b_answer(&mut ctx, sk_selector, key),
        Value::Object(Some(sel_obj))
    );
    assert_eq!(
        w11b_answer(&mut ctx, sk_attachment, key),
        Value::Object(Some(att))
    );
    assert_eq!(w11b_answer(&mut ctx, sk_is_valid, key), Value::Int(0));

    // Nothing of the key is rooted by the selector tables any more: the
    // key object carries the references, and dies with its last holder.
    let mut roots = Vec::new();
    gc_scan_selector_roots(vm, &mut roots);
    for r in [key, channel, att] {
        assert!(!roots.contains(&r), "{r:?} is still a selector-table root");
    }

    // `attach` on a retired key goes through the key's own field.
    let att2 = ctx.alloc_object(0);
    assert_eq!(
        sk_attach(
            &mut ctx,
            &[Value::Object(Some(key)), Value::Object(Some(att2))]
        )
        .unwrap(),
        Some(Value::Object(Some(att)))
    );
    assert_eq!(
        w11b_answer(&mut ctx, sk_attachment, key),
        Value::Object(Some(att2))
    );

    forget_vm_nio_rows(vm);
}

/// A key whose class cannot answer for itself (a stub-mode class with no
/// fields) keeps its row, cancelled, exactly as before the fix: removing
/// it would change what `channel()` and `isValid()` answer.
#[test]
fn w11b_a_key_that_cannot_answer_for_itself_keeps_its_row() {
    let (mut ctx, sel_obj, _id, channel) = w11b_fixture(0x5B12_0000, false);
    let vm = ctx.vm_identity();
    let key = w11b_register(&mut ctx, sel_obj, channel, None);
    key_cancel_native(&mut ctx, &[Value::Object(Some(key))]).unwrap();
    selector_select_now_native(&mut ctx, &[Value::Object(Some(sel_obj))]).unwrap();
    assert_eq!(sk_row_count(vm), 1, "a stub-mode key's row must stay");
    assert_eq!(w11b_answer(&mut ctx, sk_is_valid, key), Value::Int(0));
    assert_eq!(
        w11b_answer(&mut ctx, sk_channel, key),
        Value::Object(Some(channel))
    );
    forget_vm_nio_rows(vm);
    assert_eq!(sk_row_count(vm), 0);
}

/// Closing a channel cancels and deregisters its keys: the close path
/// retires the row at once, and the key answers `isValid() == false`.
/// The channel here was never bound, so its key sits in a placeholder
/// slot -- the sweep `deregister_channel_everywhere` owns.
#[test]
fn w11b_channel_close_retires_its_keys() {
    let (mut ctx, sel_obj, id, channel) = w11b_fixture(0x5B13_0000, true);
    let vm = ctx.vm_identity();
    let key = w11b_register(&mut ctx, sel_obj, channel, None);
    // Another channel's key on the same selector is not touched.
    let other = ctx.alloc_object(0);
    let other_key = w11b_register(&mut ctx, sel_obj, other, None);
    assert_eq!(selector_key_count(id), 2);

    deregister_channel_everywhere(&mut ctx, channel, None, Vec::new());
    assert_eq!(selector_key_count(id), 1);
    assert_eq!(
        sk_row_count(vm),
        1,
        "only the closed channel's row is retired"
    );
    assert_eq!(w11b_answer(&mut ctx, sk_is_valid, key), Value::Int(0));
    assert_eq!(
        w11b_answer(&mut ctx, sk_channel, key),
        Value::Object(Some(channel))
    );
    assert_eq!(w11b_answer(&mut ctx, sk_is_valid, other_key), Value::Int(1));

    forget_vm_nio_rows(vm);
}

/// Closing a selector deregisters and invalidates every key it held.
#[test]
fn w11b_selector_close_retires_its_keys() {
    let (mut ctx, sel_obj, _id, channel) = w11b_fixture(0x5B14_0000, true);
    let vm = ctx.vm_identity();
    let att = ctx.alloc_object(0);
    let key = w11b_register(&mut ctx, sel_obj, channel, Some(att));
    selector_close_native(&mut ctx, &[Value::Object(Some(sel_obj))]).unwrap();
    assert_eq!(sk_row_count(vm), 0);
    assert_eq!(w11b_answer(&mut ctx, sk_is_valid, key), Value::Int(0));
    assert_eq!(
        w11b_answer(&mut ctx, sk_attachment, key),
        Value::Object(Some(att))
    );
    forget_vm_nio_rows(vm);
}

/// Registering a channel with a selector it is already registered with
/// answers the SAME key (interest and attachment replaced), as the JDK
/// does; it used to mint a second key and leave the first one's row
/// behind for the life of the VM.
#[test]
fn w11b_reregistering_a_channel_answers_its_existing_key() {
    let (mut ctx, sel_obj, id, channel) = w11b_fixture(0x5B15_0000, true);
    let vm = ctx.vm_identity();
    let att = ctx.alloc_object(0);
    let k1 = w11b_register(&mut ctx, sel_obj, channel, Some(att));
    let k2 = w11b_register(&mut ctx, sel_obj, channel, None);
    assert_eq!(k1, k2, "register must answer the existing key");
    assert_eq!(sk_row_count(vm), 1);
    assert_eq!(selector_key_count(id), 1);
    assert_eq!(
        w11b_answer(&mut ctx, sk_attachment, k1),
        Value::Object(None),
        "the attachment is replaced (the 2-arg form attaches null)"
    );
    assert_eq!(w11b_answer(&mut ctx, sk_is_valid, k1), Value::Int(1));
    forget_vm_nio_rows(vm);
}

/// A key the kernel select pruned waits in `pending_retire` across the
/// kernel wait, where a collection can run: the GC hooks root and remap
/// it like a registered key.
#[test]
fn w11b_pending_retire_keys_are_scanned_and_remapped() {
    let vm: usize = 0xB11B_0001;
    let key = fake_ref(0xB1B0);
    let hash = 0x7E57_B1B1_u32 as i32;
    let id = selector_open_in(vm);
    let fd = fake_fd();
    selector_register(id, fd, OP_READ, Some(key), hash, None).unwrap();
    selector_cancel(id, fd);
    let _ = selector_select(id, 0).unwrap();
    assert_eq!(selector_key_count(id), 0, "the select pruned the key");
    assert_eq!(
        selectors_read().get(&id).unwrap().lock().pending_retire,
        vec![(key, hash)]
    );

    let mut roots = Vec::new();
    gc_scan_selector_roots(vm, &mut roots);
    assert!(roots.contains(&key), "a pending key must be rooted");

    let moved = fake_ref(0xB1B2);
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(key.as_ptr() as usize, moved.as_ptr() as usize);
    sk_table_update_after_gc(vm, &map);
    assert_eq!(
        selectors_read().get(&id).unwrap().lock().pending_retire,
        vec![(moved, hash)],
        "a pending key must be remapped"
    );

    // A ctx-less close drops the pending entries with the keys.
    selector_close(id);
    assert!(selectors_read()
        .get(&id)
        .unwrap()
        .lock()
        .pending_retire
        .is_empty());
    forget_vm_nio_rows(vm);
}
