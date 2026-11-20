use super::*;

/// The pipe table is process-wide and keyed by fd number: a test that
/// closes an fd and then asserts "not a pipe end" races another test whose
/// fresh `pipe()` reuses that number and registers it. The tests that
/// register rows run one at a time.
fn serial() -> parking_lot::MutexGuard<'static, ()> {
    static SERIAL: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    SERIAL.lock()
}

/// A pipe end is served only while its row exists, `close` defers to the
/// last thread inside, and `preClose` ends a blocked reader.
#[test]
fn r19_os_pipe_rows_close_and_pre_close() {
    let _serial = serial();
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: two-element out array.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (r, w) = (fds[0], fds[1]);
    assert!(!is_pipe_fd(r));
    register(r);
    register(w);
    assert!(is_pipe_fd(r) && is_pipe_fd(w));
    configure_blocking(r, false).unwrap();
    assert!(is_nonblocking(r));
    configure_blocking(r, true).unwrap();
    assert!(!is_nonblocking(r));
    // A thread inside defers the close.
    enter(w).unwrap();
    assert!(close(w));
    assert!(is_pipe_fd(w), "still in flight: the row stays");
    assert!(enter(w).is_none(), "closing: no new syscall");
    exit(w);
    assert!(!is_pipe_fd(w), "last one out closed it");
    // preClose: the reader's wait ends Closed; the row survives until close0.
    enter(r).unwrap();
    assert!(pre_close(r));
    assert!(closing(r));
    exit(r);
    assert!(is_pipe_fd(r));
    assert!(close(r));
    assert!(!is_pipe_fd(r));
    assert!(!close(r), "a second close is not this module's");
}

/// nio21 wave 20 L5: one lock answers "not a pipe end", "closing" or
/// entered; the one-shot `RWF_NOWAIT` syscalls answer done / `EAGAIN`, or
/// fall back where the kernel's pipes refuse the flag.
#[test]
fn w20_l5_enter_any_and_nowait() {
    let _serial = serial();
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: two-element out array.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (r, w) = (fds[0], fds[1]);
    assert!(matches!(enter_any(r), Entry::NotPipe));
    assert!(matches!(enter_any(-1), Entry::NotPipe));
    register(r);
    register(w);
    assert!(matches!(enter_any(r), Entry::Open(None)));
    let mut one = [0u8; 8];
    match rw_nowait(r, one.as_mut_ptr(), one.len(), false) {
        NoWait::Again | NoWait::Fallback => {}
        NoWait::Done(n) => panic!("an empty pipe read {n} bytes"),
    }
    let msg = *b"abc";
    match rw_nowait(w, msg.as_ptr().cast_mut(), msg.len(), true) {
        NoWait::Done(3) => match rw_nowait(r, one.as_mut_ptr(), one.len(), false) {
            NoWait::Done(3) => assert_eq!(&one[..3], b"abc"),
            _ => panic!("the written bytes are readable"),
        },
        NoWait::Fallback => {}
        _ => panic!("an empty pipe takes 3 bytes"),
    }
    exit(r);
    assert!(pre_close(r));
    assert!(matches!(enter_any(r), Entry::Closing));
    assert!(close(r));
    assert!(close(w));
}

/// nio21 wave 19 L5: the row caches the end's blocking mode (read once,
/// kept by `configure_blocking`), and the zero-timeout readiness probe
/// answers what a read / write would find.
#[test]
fn w19_l5_mode_cache_and_readiness() {
    let _serial = serial();
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: two-element out array.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (r, w) = (fds[0], fds[1]);
    register(r);
    register(w);
    let known = enter(r).unwrap();
    assert_eq!(known, None, "unknown until asked");
    assert!(!nonblocking_mode(r, known));
    exit(r);
    assert_eq!(enter(r).unwrap(), Some(false), "recorded");
    exit(r);
    configure_blocking(r, false).unwrap();
    assert_eq!(enter(r).unwrap(), Some(true));
    exit(r);
    configure_blocking(r, true).unwrap();
    assert_eq!(enter(r).unwrap(), Some(false));
    exit(r);
    assert!(!ready_now(r, libc::POLLIN), "empty: a read would block");
    assert!(ready_now(w, libc::POLLOUT), "room: a write would not");
    let b = 7u8;
    // SAFETY: one readable byte into a pipe this test owns.
    assert_eq!(unsafe { libc::write(w, (&b as *const u8).cast(), 1) }, 1);
    assert!(ready_now(r, libc::POLLIN), "a byte: a read returns at once");
    assert!(close(r));
    assert!(close(w));
}
