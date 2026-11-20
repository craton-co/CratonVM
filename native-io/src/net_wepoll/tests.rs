use super::*;

#[test]
fn event_translation_round_trips_the_bits_the_selector_uses() {
    assert_eq!(
        from_poll_events(to_poll_events(EPOLLIN | EPOLLPRI | EPOLLOUT)),
        EPOLLIN | EPOLLPRI | EPOLLOUT
    );
    assert_eq!(from_poll_events(POLLHUP), EPOLLHUP);
    assert_eq!(from_poll_events(POLLERR), EPOLLERR);
    assert_eq!(from_poll_events(POLLNVAL), EPOLLERR);
    assert_eq!(to_poll_events(0), 0);
}

#[test]
fn a_set_answers_add_mod_del_like_epoll_ctl() {
    let h = next_handle();
    sets()
        .lock()
        .insert(h, Arc::new(Mutex::new(WepollSet::default())));
    let mut ctx = crate::test_support::MockNativeContext::new();
    let ctl = |op: i32, s: i64, ev: i32, ctx: &mut dyn NativeContext| match wepoll_ctl(
        ctx,
        &[
            Value::Long(h),
            Value::Int(op),
            Value::Long(s),
            Value::Int(ev),
        ],
    ) {
        Ok(Some(Value::Int(rc))) => rc,
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(ctl(EPOLL_CTL_ADD, 7, EPOLLIN, &mut ctx), 0);
    assert_eq!(
        ctl(EPOLL_CTL_ADD, 7, EPOLLIN, &mut ctx),
        ERROR_ALREADY_EXISTS
    );
    assert_eq!(ctl(EPOLL_CTL_MOD, 7, EPOLLOUT, &mut ctx), 0);
    assert_eq!(ctl(EPOLL_CTL_MOD, 8, EPOLLOUT, &mut ctx), ERROR_NOT_FOUND);
    assert_eq!(ctl(EPOLL_CTL_DEL, 7, 0, &mut ctx), 0);
    assert_eq!(ctl(EPOLL_CTL_DEL, 7, 0, &mut ctx), ERROR_NOT_FOUND);
    sets().lock().remove(&h);
}

#[test]
fn wait_reports_a_readable_loopback_socket_by_its_java_id() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let c = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (mut a, _) = l.accept().unwrap();
    let id = register_handle(NetSocketHandle::Stream(Arc::new(c)));
    let set = Arc::new(Mutex::new(WepollSet::default()));
    set.lock().entries.insert(i64::from(id), EPOLLIN);
    assert!(poll_once(&set, 8, 0).unwrap().is_empty());
    use std::io::Write;
    a.write_all(b"x").unwrap();
    let mut ready = Vec::new();
    for _ in 0..50 {
        ready = poll_once(&set, 8, 20).unwrap();
        if !ready.is_empty() {
            break;
        }
    }
    assert_eq!(ready, vec![(i64::from(id), EPOLLIN)]);
    close_net_fd(id);
}
