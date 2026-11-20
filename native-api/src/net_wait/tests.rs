use super::*;
use std::net::TcpStream;
use std::time::{Duration, Instant};

#[test]
fn jdk_rule_matches_the_jdk_source() {
    assert_eq!(jdk_backlog(-1), 50);
    assert_eq!(jdk_backlog(0), 50);
    assert_eq!(jdk_backlog(1), 1);
    assert_eq!(jdk_backlog(1024), 1024);
}

fn queued_without_accept(listener: &TcpListener, attempts: usize) -> usize {
    let addr = listener.local_addr().unwrap();
    let mut held = Vec::new();
    for _ in 0..attempts {
        match TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
            Ok(s) => held.push(s),
            Err(_) => break,
        }
    }
    held.len()
}

/// The defect this module exists for. Windows caps the queue at 200, so the
/// assertion is "more than std's 128", not "== requested".
#[test]
fn a_large_requested_backlog_queues_more_than_std_would() {
    let listener = bind_tcp_listener("127.0.0.1:0".parse().unwrap(), 1024).unwrap();
    let queued = queued_without_accept(&listener, 180);
    assert!(
        queued > STD_BACKLOG as usize,
        "only {queued} queued with backlog 1024"
    );
}

#[test]
fn a_small_requested_backlog_is_not_widened_to_std_s() {
    let listener = bind_tcp_listener("127.0.0.1:0".parse().unwrap(), 5).unwrap();
    let queued = queued_without_accept(&listener, 60);
    assert!(queued < 60, "backlog 5 queued all {queued} connects");
}

#[test]
fn deferred_listen_on_an_unlistened_listener_applies_the_backlog() {
    let socket = bind_tcp_unlistened("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener: TcpListener = socket.into();
    listen_existing(&listener, 1024).unwrap();
    let queued = queued_without_accept(&listener, 180);
    assert!(
        queued > STD_BACKLOG as usize,
        "only {queued} queued after deferred listen"
    );
}

#[test]
fn wait_returns_when_a_connection_is_pending_not_at_the_timeout() {
    let listener = bind_tcp_listener("127.0.0.1:0".parse().unwrap(), 50).unwrap();
    let addr = listener.local_addr().unwrap();
    let raw = raw_sock(&listener);
    let client = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        TcpStream::connect(addr).unwrap()
    });
    let start = Instant::now();
    let ready = wait_readable_raw(raw, 5_000);
    let waited = start.elapsed();
    let _keep = client.join().unwrap();
    assert!(ready);
    assert!(waited < Duration::from_millis(2_000), "waited {waited:?}");
}

#[test]
fn wait_times_out_quietly_on_an_idle_listener() {
    let listener = bind_tcp_listener("127.0.0.1:0".parse().unwrap(), 50).unwrap();
    let start = Instant::now();
    assert!(!wait_readable_raw(raw_sock(&listener), 30));
    assert!(start.elapsed() >= Duration::from_millis(20));
}
