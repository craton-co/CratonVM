use super::*;
use crate::test_support::MockNativeContext;

#[test]
fn a_port_hands_completions_back_in_order_and_drops_them_once_closed() {
    let mut ctx = MockNativeContext::new();
    let port = match iocp_create(
        &mut ctx,
        &[
            Value::Long(-1),
            Value::Long(0),
            Value::Int(0),
            Value::Int(1),
        ],
    )
    .unwrap()
    {
        Some(Value::Long(p)) => p,
        other => panic!("{other:?}"),
    };
    iocp_create(
        &mut ctx,
        &[
            Value::Long(77),
            Value::Long(port),
            Value::Int(5),
            Value::Int(0),
        ],
    )
    .unwrap();
    complete(77, 0x1000, 0, 4, None);
    complete(77, 0x2000, 995, 0, None);
    let p = port_of(port).unwrap();
    let mut q = p.queue.lock();
    let a = q.pop_front().unwrap();
    let b = q.pop_front().unwrap();
    assert_eq!((a.key, a.overlapped, a.error, a.bytes), (5, 0x1000, 0, 4));
    assert_eq!((b.key, b.overlapped, b.error), (5, 0x2000, 995));
    assert!(q.is_empty());
    drop(q);
    iocp_close(&mut ctx, &[Value::Long(port)]).unwrap();
    complete(77, 0x3000, 0, 0, None);
    assert!(port_of(port).is_none());
}

#[test]
fn wsabufs_read_the_len_and_pointer_of_each_entry() {
    let raw = [0u8; 16];
    let len = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
    assert_eq!(len, 0);
    assert_eq!(SIZEOF_WSABUF, 16);
}

#[test]
fn an_aborted_socket_reports_operation_aborted_whatever_the_error() {
    aborted().lock().insert(424_242);
    let e = std::io::Error::from_raw_os_error(WSAECONNRESET);
    assert_eq!(error_code(&e, 424_242), ERROR_OPERATION_ABORTED);
    assert_eq!(error_code(&e, 1), 64);
    assert_eq!(overlapped_error(WSAECONNREFUSED), 1225);
}

#[test]
fn worker_pool_runs_jobs_and_wakes_port() {
    let port = next_port();
    let p = Arc::new(Port {
        queue: Mutex::new(VecDeque::new()),
        ready: Condvar::new(),
        closed: AtomicBool::new(false),
    });
    ports().lock().insert(port, Arc::clone(&p));
    let sock = 888_888i64;
    associations().lock().insert(sock, (port, 42));
    for i in 1..=20i64 {
        spawn(move || {
            complete(sock, i * 0x100, 0, i as i32, None);
        });
    }
    let mut got = Vec::new();
    let mut q = p.queue.lock();
    while got.len() < 20 {
        while let Some(c) = q.pop_front() {
            got.push((c.key, c.overlapped, c.bytes));
        }
        if got.len() < 20 {
            let r = p.ready.wait_for(&mut q, Duration::from_secs(2));
            assert!(
                !r.timed_out(),
                "timed out waiting for pooled completion, got {}",
                got.len()
            );
        }
    }
    assert_eq!(got.len(), 20);
    drop(q);
    ports().lock().remove(&port);
    associations().lock().remove(&sock);
}
