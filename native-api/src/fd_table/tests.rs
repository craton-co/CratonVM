use super::*;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering as AtomOrd};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The i53 residual: a single-byte `FileOutputStream.write(int)` reaches
/// the OS at once, as `write_bytes` does: another handle reads it
/// before the writer closes.
#[test]
fn i53r_a_written_byte_is_visible_before_close() {
    let path = temp_file_with("");
    let table = FileDescriptorTable::new();
    let w = table.open_write(&path, false).unwrap();
    table.write_byte(w, 42).unwrap();
    assert_eq!(fs::read(&path).unwrap(), [42]);
    table.close(w).unwrap();
    let _ = fs::remove_file(&path);
}

/// Helper: create a temp file with the given content and return its path.
fn temp_file_with(content: &str) -> String {
    let id = TEST_COUNTER.fetch_add(1, AtomOrd::Relaxed);
    let dir = std::env::temp_dir();
    let name = format!("cratonvm_fdtest_{}_{}.txt", std::process::id(), id);
    let path = dir.join(name);
    let mut f = fs::File::create(&path).unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    drop(f);
    path.to_string_lossy().into_owned()
}

/// Helper: create a unique temp file path (file may or may not exist).
fn temp_path(suffix: &str) -> String {
    let id = TEST_COUNTER.fetch_add(1, AtomOrd::Relaxed);
    let dir = std::env::temp_dir();
    let name = format!(
        "cratonvm_fdtest_{}_{}_{}.txt",
        std::process::id(),
        id,
        suffix
    );
    dir.join(name).to_string_lossy().into_owned()
}

// -----------------------------------------------------------------------
// Construction
// -----------------------------------------------------------------------

#[test]
fn new_table_has_three_standard_fds() {
    let table = FileDescriptorTable::new();
    // stdin(0), stdout(1), stderr(2) are pre-registered
    assert_eq!(table.entries.read().len(), 3);
}

#[test]
fn default_is_same_as_new() {
    let table = FileDescriptorTable::default();
    assert_eq!(table.entries.read().len(), 3);
}

#[test]
fn debug_format_shows_open_fds() {
    let table = FileDescriptorTable::new();
    let dbg = format!("{:?}", table);
    assert!(dbg.contains("open_fds: 3"));
}

// -----------------------------------------------------------------------
// open_read
// -----------------------------------------------------------------------

#[test]
fn open_read_returns_fd_ge_3() {
    let path = temp_file_with("hello");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(&path).unwrap();
    assert!(fd >= 3);
    let _ = fs::remove_file(&path);
}

#[test]
fn open_read_nonexistent_file_errors() {
    let table = FileDescriptorTable::new();
    let result = table.open_read("/tmp/cratonvm_fdtest_nonexistent_xyzzy.txt");
    assert!(result.is_err());
}

// -----------------------------------------------------------------------
// open_write
// -----------------------------------------------------------------------

#[test]
fn open_write_creates_file() {
    let path = temp_path("write_create");
    let _ = fs::remove_file(&path);
    let table = FileDescriptorTable::new();
    let fd = table.open_write(&path, false).unwrap();
    assert!(fd >= 3);
    assert!(fs::metadata(&path).is_ok());
    let _ = fs::remove_file(&path);
}

#[test]
fn open_write_truncate_mode() {
    let path = temp_file_with("old content");
    let table = FileDescriptorTable::new();
    let fd = table.open_write(&path, false).unwrap();
    table.write_string(fd, "new").unwrap();
    table.flush(fd).unwrap();
    table.close(fd).unwrap();
    let content = fs::read_to_string(&path).unwrap();
    assert_eq!(content, "new");
    let _ = fs::remove_file(&path);
}

#[test]
fn open_write_append_mode() {
    let path = temp_file_with("first");
    let table = FileDescriptorTable::new();
    let fd = table.open_write(&path, true).unwrap();
    table.write_string(fd, "second").unwrap();
    table.flush(fd).unwrap();
    table.close(fd).unwrap();
    let content = fs::read_to_string(&path).unwrap();
    assert_eq!(content, "firstsecond");
    let _ = fs::remove_file(&path);
}

// -----------------------------------------------------------------------
// open_write_options (F37-1 NOM N2) — `create`, `truncate` and `append`
// exactly as asked, independent of each other. `open_write` above cannot
// express these rows at all: it always creates and always truncates
// unless `append`.
// -----------------------------------------------------------------------

/// MEASURED (F37-1 §5.2): `FileChannel.open(missing, WRITE)` — writable,
/// no CREATE — raises `NoSuchFileException`. `open_write` would have
/// created the file unconditionally.
#[test]
fn open_write_options_without_create_refuses_a_missing_file() {
    let path = temp_path("write_options_no_create");
    let _ = fs::remove_file(&path);
    let table = FileDescriptorTable::new();
    let err = table
        .open_write_options(&path, false, false, false)
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert!(fs::metadata(&path).is_err(), "must not have created it");
}

/// MEASURED: `open(missing, WRITE, CREATE)` succeeds with `size() == 0`.
#[test]
fn open_write_options_with_create_makes_an_empty_file() {
    let path = temp_path("write_options_create");
    let _ = fs::remove_file(&path);
    let table = FileDescriptorTable::new();
    let fd = table.open_write_options(&path, true, false, false).unwrap();
    assert_eq!(table.file_size(fd).unwrap(), 0);
    table.close(fd).unwrap();
    let _ = fs::remove_file(&path);
}

/// MEASURED: `open(existing, WRITE)` then a one-byte write leaves the
/// file's length UNCHANGED — WRITE alone neither creates nor truncates.
/// The naive `open_write(path, false)` mapping (`.create(true)
/// .truncate(true)`) would have truncated it to 1 byte.
#[test]
fn open_write_options_without_truncate_preserves_existing_length() {
    let path = temp_file_with("hello");
    let table = FileDescriptorTable::new();
    let fd = table
        .open_write_options(&path, false, false, false)
        .unwrap();
    table.write_bytes(fd, b"X").unwrap();
    assert_eq!(table.file_size(fd).unwrap(), 5, "length must be unchanged");
    table.close(fd).unwrap();
    let content = fs::read(&path).unwrap();
    assert_eq!(&content[..1], b"X", "the write itself still landed");
    let _ = fs::remove_file(&path);
}

/// `truncate: true` (WRITE + TRUNCATE_EXISTING) does what it says on an
/// existing file.
#[test]
fn open_write_options_truncate_empties_an_existing_file() {
    let path = temp_file_with("old content");
    let table = FileDescriptorTable::new();
    let fd = table.open_write_options(&path, false, true, false).unwrap();
    assert_eq!(table.file_size(fd).unwrap(), 0);
    table.close(fd).unwrap();
    let _ = fs::remove_file(&path);
}

/// `append: true` overrides a `truncate: true` request — matching
/// `open_write`'s existing `.truncate(!append)` rule, and the reason
/// `open_write_options` computes `truncate && !append` rather than taking
/// the caller's `truncate` bit literally.
#[test]
fn open_write_options_append_wins_over_truncate() {
    let path = temp_file_with("keep");
    let table = FileDescriptorTable::new();
    let fd = table.open_write_options(&path, false, true, true).unwrap();
    table.write_bytes(fd, b"+more").unwrap();
    table.close(fd).unwrap();
    let content = fs::read_to_string(&path).unwrap();
    assert_eq!(content, "keep+more");
    let _ = fs::remove_file(&path);
}

// -----------------------------------------------------------------------
// read_line
// -----------------------------------------------------------------------

#[test]
fn read_line_returns_lines_without_newline() {
    let path = temp_file_with("line1\nline2\nline3\n");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(&path).unwrap();
    assert_eq!(table.read_line(fd).unwrap(), Some("line1".to_string()));
    assert_eq!(table.read_line(fd).unwrap(), Some("line2".to_string()));
    assert_eq!(table.read_line(fd).unwrap(), Some("line3".to_string()));
    assert_eq!(table.read_line(fd).unwrap(), None); // EOF
    let _ = fs::remove_file(&path);
}

#[test]
fn read_line_strips_crlf() {
    let path = temp_file_with("hello\r\nworld\r\n");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(&path).unwrap();
    assert_eq!(table.read_line(fd).unwrap(), Some("hello".to_string()));
    assert_eq!(table.read_line(fd).unwrap(), Some("world".to_string()));
    let _ = fs::remove_file(&path);
}

#[test]
fn read_line_bad_fd_errors() {
    let table = FileDescriptorTable::new();
    let result = table.read_line(999);
    assert!(result.is_err());
}

// -----------------------------------------------------------------------
// read_byte / read_bytes
// -----------------------------------------------------------------------

#[test]
fn read_byte_returns_bytes_then_eof() {
    let path = temp_file_with("AB");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(&path).unwrap();
    assert_eq!(table.read_byte(fd).unwrap(), b'A' as i32);
    assert_eq!(table.read_byte(fd).unwrap(), b'B' as i32);
    assert_eq!(table.read_byte(fd).unwrap(), -1); // EOF
    let _ = fs::remove_file(&path);
}

#[test]
fn read_bytes_fills_buffer() {
    let path = temp_file_with("hello world");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(&path).unwrap();
    let mut buf = [0u8; 5];
    let n = table.read_bytes(fd, &mut buf).unwrap();
    assert_eq!(n, 5);
    assert_eq!(&buf, b"hello");
    let _ = fs::remove_file(&path);
}

#[test]
fn read_byte_bad_fd_errors() {
    let table = FileDescriptorTable::new();
    assert!(table.read_byte(999).is_err());
}

#[test]
fn read_bytes_bad_fd_errors() {
    let table = FileDescriptorTable::new();
    let mut buf = [0u8; 4];
    assert!(table.read_bytes(999, &mut buf).is_err());
}

// -----------------------------------------------------------------------
// write_byte / write_bytes / write_string
// -----------------------------------------------------------------------

#[test]
fn write_byte_to_file() {
    let path = temp_path("write_byte");
    let table = FileDescriptorTable::new();
    let fd = table.open_write(&path, false).unwrap();
    table.write_byte(fd, b'X').unwrap();
    table.flush(fd).unwrap();
    table.close(fd).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "X");
    let _ = fs::remove_file(&path);
}

#[test]
fn write_bytes_to_file() {
    let path = temp_path("write_bytes");
    let table = FileDescriptorTable::new();
    let fd = table.open_write(&path, false).unwrap();
    table.write_bytes(fd, b"hello").unwrap();
    table.flush(fd).unwrap();
    table.close(fd).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
    let _ = fs::remove_file(&path);
}

#[test]
fn write_string_to_file() {
    let path = temp_path("write_string");
    let table = FileDescriptorTable::new();
    let fd = table.open_write(&path, false).unwrap();
    table.write_string(fd, "rust jvm").unwrap();
    table.flush(fd).unwrap();
    table.close(fd).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "rust jvm");
    let _ = fs::remove_file(&path);
}

#[test]
fn write_byte_bad_fd_errors() {
    let table = FileDescriptorTable::new();
    assert!(table.write_byte(999, b'X').is_err());
}

#[test]
fn write_bytes_bad_fd_errors() {
    let table = FileDescriptorTable::new();
    assert!(table.write_bytes(999, b"data").is_err());
}

// -----------------------------------------------------------------------
// close
// -----------------------------------------------------------------------

#[test]
fn close_removes_fd() {
    let path = temp_file_with("data");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(&path).unwrap();
    table.close(fd).unwrap();
    // After close, reading should fail
    assert!(table.read_byte(fd).is_err());
    let _ = fs::remove_file(&path);
}

#[test]
fn close_already_closed_fd_is_noop() {
    let path = temp_file_with("data");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(&path).unwrap();
    table.close(fd).unwrap();
    // Second close should not panic
    table.close(fd).unwrap();
    let _ = fs::remove_file(&path);
}

#[test]
fn close_does_not_remove_stdin_stdout_stderr() {
    let table = FileDescriptorTable::new();
    table.close(0).unwrap();
    table.close(1).unwrap();
    table.close(2).unwrap();
    // Standard fds should still be present
    assert_eq!(table.entries.read().len(), 3);
}

// -----------------------------------------------------------------------
// flush
// -----------------------------------------------------------------------

#[test]
fn flush_nonwritable_fd_is_noop() {
    let path = temp_file_with("data");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(&path).unwrap();
    // flush on a read fd should be Ok (no-op for non-writable)
    assert!(table.flush(fd).is_ok());
    let _ = fs::remove_file(&path);
}

#[test]
fn flush_nonexistent_fd_is_noop() {
    let table = FileDescriptorTable::new();
    assert!(table.flush(999).is_ok());
}

// -----------------------------------------------------------------------
// available
// -----------------------------------------------------------------------

#[test]
fn available_on_read_fd() {
    let path = temp_file_with("hello");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(&path).unwrap();
    // Before any read, buffer may be empty (0) — that's valid
    let avail = table.available(fd).unwrap();
    let _ = avail; // just checking available() didn't error
                   // After a read, there may be more in the buffer
    let _ = table.read_byte(fd);
    let _ = table.available(fd).unwrap();
    let _ = fs::remove_file(&path);
}

/// nio20 wave 3, L1: a read-write fd answers size minus cursor (libnio's
/// regular-file `available`), not the `bad fd` error that read as 0.
#[test]
fn available_on_read_write_fd_is_size_minus_cursor() {
    let path = temp_file_with("0123456789");
    let table = FileDescriptorTable::new();
    let fd = table.open_read_write(&path, false).unwrap();
    assert_eq!(table.available(fd).unwrap(), 10);
    let _ = table.read_byte(fd).unwrap();
    let _ = table.read_byte(fd).unwrap();
    let _ = table.read_byte(fd).unwrap();
    assert_eq!(table.available(fd).unwrap(), 7);
    let _ = table.close(fd);
    let _ = fs::remove_file(&path);
}

#[test]
fn available_bad_fd_errors() {
    let table = FileDescriptorTable::new();
    assert!(table.available(999).is_err());
}

#[test]
fn available_stdin_returns_zero() {
    let table = FileDescriptorTable::new();
    assert_eq!(table.available(0).unwrap(), 0);
}

#[test]
fn pipe_write_caps_buffer_and_reports_backpressure() {
    let table = FileDescriptorTable::new();
    let (read_fd, write_fd) = table.open_pipe();

    let almost_full = vec![0x41; PIPE_BUFFER_CAPACITY - 1];
    assert_eq!(
        table.pipe_write(write_fd, &almost_full).unwrap(),
        PIPE_BUFFER_CAPACITY - 1
    );
    assert_eq!(table.poll_ready(write_fd), (false, true));

    assert_eq!(table.pipe_write(write_fd, b"xy").unwrap(), 1);
    assert_eq!(table.poll_ready(write_fd), (false, false));

    let err = table.pipe_write(write_fd, b"z").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(table.pipe_write(write_fd, b"").unwrap(), 0);

    let mut one = [0u8; 1];
    assert_eq!(table.pipe_read(read_fd, &mut one).unwrap(), 1);
    assert_eq!(table.poll_ready(write_fd), (false, true));
    assert_eq!(table.pipe_write(write_fd, b"z").unwrap(), 1);
}

// -----------------------------------------------------------------------
// Multiple concurrent opens
// -----------------------------------------------------------------------

#[test]
fn multiple_concurrent_opens_get_unique_fds() {
    let p1 = temp_file_with("a");
    let p2 = temp_file_with("b");
    let p3 = temp_file_with("c");
    let table = FileDescriptorTable::new();
    let fd1 = table.open_read(&p1).unwrap();
    let fd2 = table.open_read(&p2).unwrap();
    let fd3 = table.open_read(&p3).unwrap();
    assert_ne!(fd1, fd2);
    assert_ne!(fd2, fd3);
    assert_ne!(fd1, fd3);
    assert!(fd1 >= 3 && fd2 >= 3 && fd3 >= 3);
    let _ = fs::remove_file(&p1);
    let _ = fs::remove_file(&p2);
    let _ = fs::remove_file(&p3);
}

// -----------------------------------------------------------------------
// FD counter overflow protection
// -----------------------------------------------------------------------

#[test]
fn fd_overflow_protection_read() {
    let path = temp_file_with("overflow test");
    let table = FileDescriptorTable::new();
    // Force the counter near the limit
    table.next_fd.store(u32::MAX - 10, Ordering::Relaxed);
    let result = table.open_read(&path);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Other);
    assert!(err.to_string().contains("file descriptor limit exceeded"));
    let _ = fs::remove_file(&path);
}

#[test]
fn fd_overflow_protection_write() {
    let path = temp_path("overflow_write");
    let table = FileDescriptorTable::new();
    table.next_fd.store(u32::MAX - 10, Ordering::Relaxed);
    let result = table.open_write(&path, false);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("file descriptor limit exceeded"));
    let _ = fs::remove_file(&path);
}

// -----------------------------------------------------------------------
// Round-trip: write then read
// -----------------------------------------------------------------------

#[test]
fn write_then_read_roundtrip() {
    let path = temp_path("roundtrip");
    let table = FileDescriptorTable::new();

    // Write
    let wfd = table.open_write(&path, false).unwrap();
    table.write_string(wfd, "line1\nline2\n").unwrap();
    table.flush(wfd).unwrap();
    table.close(wfd).unwrap();

    // Read back
    let rfd = table.open_read(&path).unwrap();
    assert_eq!(table.read_line(rfd).unwrap(), Some("line1".to_string()));
    assert_eq!(table.read_line(rfd).unwrap(), Some("line2".to_string()));
    assert_eq!(table.read_line(rfd).unwrap(), None);
    table.close(rfd).unwrap();
    let _ = fs::remove_file(&path);
}

// -----------------------------------------------------------------------
// Write to stdout/stderr fds (smoke test — just ensure no panic/error)
// -----------------------------------------------------------------------

#[test]
fn write_to_stdout_fd() {
    let table = FileDescriptorTable::new();
    // Writing to stdout fd should succeed (output goes to test harness)
    assert!(table.write_byte(1, b'\n').is_ok());
    assert!(table.write_bytes(1, b"").is_ok());
    assert!(table.write_string(1, "").is_ok());
}

#[test]
fn write_to_stderr_fd() {
    let table = FileDescriptorTable::new();
    assert!(table.write_byte(2, b'\n').is_ok());
}

#[test]
fn flush_stdout_stderr() {
    let table = FileDescriptorTable::new();
    assert!(table.flush(1).is_ok());
    assert!(table.flush(2).is_ok());
}

// -----------------------------------------------------------------------
// Read from write fd should fail
// -----------------------------------------------------------------------

#[test]
fn read_from_write_fd_errors() {
    let path = temp_path("read_write_fd");
    let table = FileDescriptorTable::new();
    let fd = table.open_write(&path, false).unwrap();
    assert!(table.read_byte(fd).is_err());
    assert!(table.read_line(fd).is_err());
    table.close(fd).unwrap();
    let _ = fs::remove_file(&path);
}

#[test]
fn write_to_read_fd_errors() {
    let path = temp_file_with("data");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(&path).unwrap();
    assert!(table.write_byte(fd, b'X').is_err());
    assert!(table.write_bytes(fd, b"X").is_err());
    table.close(fd).unwrap();
    let _ = fs::remove_file(&path);
}

// -----------------------------------------------------------------------
// T2.4.14 — BufferedReader.readLine: must recognize \n, \r\n, and
// bare \r as line terminators (BufferedReader javadoc contract).
// -----------------------------------------------------------------------

#[test]
fn t2_read_line_unix_lf() {
    let path = temp_file_with("alpha\nbeta\n");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(&path).unwrap();
    assert_eq!(table.read_line(fd).unwrap(), Some("alpha".into()));
    assert_eq!(table.read_line(fd).unwrap(), Some("beta".into()));
    assert_eq!(table.read_line(fd).unwrap(), None);
    table.close(fd).unwrap();
    let _ = fs::remove_file(&path);
}

#[test]
fn t2_read_line_windows_crlf_stays_atomic() {
    let path = temp_file_with("one\r\ntwo\r\nthree");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(&path).unwrap();
    assert_eq!(table.read_line(fd).unwrap(), Some("one".into()));
    assert_eq!(table.read_line(fd).unwrap(), Some("two".into()));
    // Final line without terminator still produces a value.
    assert_eq!(table.read_line(fd).unwrap(), Some("three".into()));
    assert_eq!(table.read_line(fd).unwrap(), None);
    table.close(fd).unwrap();
    let _ = fs::remove_file(&path);
}

#[test]
fn t2_read_line_classic_mac_cr_only() {
    // Classic Mac OS used a bare \r as the line terminator. The
    // previous implementation returned the entire file as a single
    // line because `BufRead::read_line` only stops on \n.
    let path = temp_file_with("uno\rdos\rtres");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(&path).unwrap();
    assert_eq!(table.read_line(fd).unwrap(), Some("uno".into()));
    assert_eq!(table.read_line(fd).unwrap(), Some("dos".into()));
    assert_eq!(table.read_line(fd).unwrap(), Some("tres".into()));
    assert_eq!(table.read_line(fd).unwrap(), None);
    table.close(fd).unwrap();
    let _ = fs::remove_file(&path);
}

#[test]
fn t2_read_line_mixed_terminators() {
    // Verifies interleaved \n / \r / \r\n all work in the same file.
    let path = temp_file_with("a\nb\rc\r\nd");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(&path).unwrap();
    assert_eq!(table.read_line(fd).unwrap(), Some("a".into()));
    assert_eq!(table.read_line(fd).unwrap(), Some("b".into()));
    assert_eq!(table.read_line(fd).unwrap(), Some("c".into()));
    assert_eq!(table.read_line(fd).unwrap(), Some("d".into()));
    assert_eq!(table.read_line(fd).unwrap(), None);
    table.close(fd).unwrap();
    let _ = fs::remove_file(&path);
}

#[test]
fn t2_read_line_empty_line_returns_empty_string() {
    // Consecutive newlines should each yield an empty line, not get
    // collapsed. Same invariant as BufferedReader.readLine.
    let path = temp_file_with("\n\n");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(&path).unwrap();
    assert_eq!(table.read_line(fd).unwrap(), Some(String::new()));
    assert_eq!(table.read_line(fd).unwrap(), Some(String::new()));
    assert_eq!(table.read_line(fd).unwrap(), None);
    table.close(fd).unwrap();
    let _ = fs::remove_file(&path);
}

// -----------------------------------------------------------------------
// tcp_available — the previous implementation toggled the socket's
// non-blocking flag (set true / peek / set false), which silently
// clobbered a non-blocking flag the caller had deliberately set.
// See the comment at `fd_table.rs:1001` for the analogous rewrite of
// `poll_ready` that this fix mirrors.
// -----------------------------------------------------------------------

/// Helper: spin up a localhost loopback TcpStream pair and return
/// the client side (so the test owns one end and can probe it).
/// The server-side accepted socket is kept alive in the returned
/// guard so the connection doesn't reset under the test.
fn loopback_pair() -> (
    std::net::TcpStream,
    std::net::TcpListener,
    std::net::TcpStream,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = std::net::TcpStream::connect(addr).unwrap();
    let (server, _) = listener.accept().unwrap();
    (client, listener, server)
}

/// The wildcard TCP listener has to accept BOTH loopback families, and it
/// has to say so in `local_addr()`.
///
/// The regression this pins is one line of behaviour, not one line of code:
/// `TcpListener::bind("0.0.0.0:0")` is an AF_INET listener, and a `::1`
/// client of one gets `Connection refused`. That is what all 48
/// parameterisations of `SSLEngineTest.testMutualAuthDiffCerts` hit, in four
/// netty SSL classes, once `NetUtil.LOCALHOST` resolved to the IPv6
/// loopback.
///
/// Skipped rather than failed on a host with no IPv6 stack: the helper's
/// documented fallback is a v4 wildcard, and asserting the v6 half there
/// would be asserting the host's configuration.
#[test]
fn dual_stack_wildcard_listener_accepts_both_loopback_families() {
    let listener = match open_tcp_dual_stack_listener(0, 0) {
        Ok(l) => l,
        Err(_) => return,
    };
    let local = listener.local_addr().expect("local_addr");
    if local.is_ipv4() {
        // No IPv6 on this host — the documented fallback. Nothing to assert.
        return;
    }
    assert!(
        local.ip().is_unspecified(),
        "expected the wildcard, got {local}"
    );
    let port = local.port();

    for host in ["127.0.0.1", "::1"] {
        let target: std::net::SocketAddr = if host.contains(':') {
            format!("[{host}]:{port}").parse().unwrap()
        } else {
            format!("{host}:{port}").parse().unwrap()
        };
        let client = std::net::TcpStream::connect(target)
            .unwrap_or_else(|e| panic!("dual-stack listener refused {target}: {e}"));
        let (server, _) = listener.accept().expect("accept");
        drop(server);
        drop(client);
    }
}

#[test]
fn poll_ready_tcp_reports_os_read_write_bits() {
    let (client, _listener, _server) = loopback_pair();
    let table = FileDescriptorTable::new();
    let fd = table.insert_tcp_stream(client);

    let expected = {
        let entry = table.get_entry(fd).unwrap();
        match &*entry {
            FileEntry::TcpStream(stream) => {
                let stream = stream.lock();
                poll_socket_readiness(&*stream)
            }
            _ => unreachable!(),
        }
    };
    assert_eq!(table.poll_ready(fd), expected);
}

#[test]
fn poll_ready_udp_reports_os_read_write_bits() {
    let table = FileDescriptorTable::new();
    let fd = table.open_udp(Some("127.0.0.1:0")).unwrap();

    let expected = {
        let entry = table.get_entry(fd).unwrap();
        match &*entry {
            FileEntry::UdpSocket(sock) => poll_socket_readiness(sock),
            _ => unreachable!(),
        }
    };
    assert_eq!(table.poll_ready(fd), expected);
}

/// Differentiates a non-blocking socket from a blocking one by
/// timing: read on an empty buffer returns `WouldBlock` on *both*
/// paths (set_read_timeout also surfaces a timeout as WouldBlock
/// on Linux), so we time how long the read took. A truly non-
/// blocking read returns essentially immediately; a blocking read
/// gated by a 200ms timeout returns after ~200ms.
fn is_still_nonblocking_via_timing(stream: &std::net::TcpStream) -> bool {
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut buf = [0u8; 16];
    let start = std::time::Instant::now();
    // We expect this to fail (empty buffer); we only care about
    // *how long* it took to fail. `impl Read for &TcpStream`
    // lets us read through a shared reference.
    let mut reader: &std::net::TcpStream = stream;
    let _ = reader.read(&mut buf);
    let elapsed = start.elapsed();
    // Clear the timeout so we don't perturb later reads in the
    // same test.
    stream.set_read_timeout(None).unwrap();
    // Non-blocking: well under 50ms. Blocking-with-timeout: ~200ms.
    // 100ms is a comfortable midpoint.
    elapsed < Duration::from_millis(100)
}

#[test]
fn tcp_available_preserves_nonblocking_flag_no_data() {
    // Regression: pre-fix `tcp_available` did
    //   set_nonblocking(true); peek(); set_nonblocking(false);
    // which silently clobbered an intentionally non-blocking
    // socket. The new code uses `poll_socket_readiness` first and
    // peeks only when the kernel says there's data — leaving the
    // blocking flag untouched. See `fd_table.rs:1001` for the
    // analogous fix that `poll_ready` already received.
    let (client, _listener, _server) = loopback_pair();
    let table = FileDescriptorTable::new();
    let fd = table.insert_tcp_stream(client);

    // Caller persistently sets non-blocking.
    table.tcp_set_nonblocking(fd, true).unwrap();

    // Probe — there is no data on the wire so this returns Ok(0).
    let avail = table.tcp_available(fd).unwrap();
    assert_eq!(avail, 0, "no data was sent, available should be 0");

    // The persistent non-blocking flag must still be in effect.
    let entry = table.get_entry(fd).unwrap();
    match &*entry {
        FileEntry::TcpStream(s) => {
            let stream = s.lock();
            assert!(
                is_still_nonblocking_via_timing(&*stream),
                "tcp_available clobbered the persistent non-blocking flag"
            );
        }
        _ => unreachable!(),
    }
}

#[test]
fn tcp_available_preserves_nonblocking_flag_with_data() {
    // Same regression check but with bytes already buffered: the
    // poll path reports "readable" and we hit the peek branch. The
    // non-blocking flag must still survive intact.
    let (client, _listener, mut server) = loopback_pair();
    // Push some bytes from the server end so the client's recv
    // buffer has data ready.
    server.write_all(b"hello world").unwrap();
    server.flush().unwrap();

    let table = FileDescriptorTable::new();
    let fd = table.insert_tcp_stream(client);
    table.tcp_set_nonblocking(fd, true).unwrap();

    // Give the kernel a brief moment to deliver the bytes to the
    // client's recv buffer so `poll` reports readable.
    let mut avail = 0;
    for _ in 0..50 {
        avail = table.tcp_available(fd).unwrap();
        if avail > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(avail > 0, "expected buffered bytes, got {avail}");

    // Persistent non-blocking flag must still be set. Drain the
    // buffered bytes first so the timing check sees an empty
    // recv buffer.
    let entry = table.get_entry(fd).unwrap();
    match &*entry {
        FileEntry::TcpStream(s) => {
            let mut stream = s.lock();
            let mut buf = [0u8; 64];
            let _ = stream.read(&mut buf).unwrap();
            assert!(
                is_still_nonblocking_via_timing(&*stream),
                "tcp_available clobbered the persistent non-blocking flag \
                 after the data-present (peek) path"
            );
        }
        _ => unreachable!(),
    }
}

// -----------------------------------------------------------------------
// Capability-checked openers
// -----------------------------------------------------------------------

use crate::capability::{Capability, CapabilityMode, CapabilitySet, Scope, VmId};

fn caps(mode: CapabilityMode, grants: &str) -> CapabilitySet {
    let mut set = CapabilitySet::new(VmId::from_raw(0xFD_0001), mode);
    assert!(
        set.grant_from_list(grants).is_empty(),
        "test grant list must parse"
    );
    set
}

#[test]
fn permissive_checked_openers_behave_exactly_like_the_raw_ones() {
    let path = temp_file_with("hello");
    let table = FileDescriptorTable::new();
    let policy = caps(CapabilityMode::Permissive, "");
    let fd = table
        .open_read_checked(&policy, &path)
        .expect("permissive allows everything");
    let mut buf = [0u8; 8];
    let n = table.read_bytes(fd, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"hello");
    table.close(fd).unwrap();
    let _ = fs::remove_file(&path);
}

#[test]
fn enforce_denies_an_ungranted_read_without_touching_the_filesystem() {
    let path = temp_file_with("secret");
    // Sequential ids: the assertion below counts reservations
    // (`CRATONVM_FD_TABLE_OS_KEYS` keys by OS value instead).
    let table = FileDescriptorTable::new_with_os_keys(false);
    let policy = caps(CapabilityMode::Enforce, "file-read:/definitely/elsewhere");

    let err = table
        .open_read_checked(&policy, &path)
        .expect_err("path is outside the grant");
    match err {
        FdCapabilityError::Denied(d) => {
            assert_eq!(d.capability, crate::capability::CapabilityKind::FileRead);
            assert_eq!(d.vm, VmId::from_raw(0xFD_0001));
        }
        FdCapabilityError::Io(e) => panic!("expected a denial, got io error {e}"),
    }
    // No fd was consumed by the refused open: the next successful open
    // gets the very next descriptor after the reserved 0/1/2.
    let permissive = caps(CapabilityMode::Permissive, "");
    let fd = table.open_read_checked(&permissive, &path).unwrap();
    assert_eq!(fd, 3, "a denied open must not reserve a descriptor");
    table.close(fd).unwrap();
    let _ = fs::remove_file(&path);
}

#[test]
fn enforce_denies_a_write_that_would_have_created_the_file() {
    let path = temp_path("cap_denied_create");
    assert!(!std::path::Path::new(&path).exists());
    let table = FileDescriptorTable::new();
    let policy = caps(CapabilityMode::Enforce, "file-read:*");

    assert!(matches!(
        table.open_write_checked(&policy, &path, false),
        Err(FdCapabilityError::Denied(_))
    ));
    assert!(
        !std::path::Path::new(&path).exists(),
        "a denied write must not create the file — the check runs before the syscall"
    );
}

#[test]
fn a_granted_path_prefix_admits_files_under_it() {
    let path = temp_file_with("granted");
    let dir = std::env::temp_dir();
    let table = FileDescriptorTable::new();
    let mut policy = CapabilitySet::new(VmId::from_raw(0xFD_0002), CapabilityMode::Enforce);
    policy.grant(Capability::FileRead(Scope::path(&dir.to_string_lossy())));

    let fd = table
        .open_read_checked(&policy, &path)
        .expect("the temp dir prefix must admit a file inside it");
    table.close(fd).unwrap();

    // ...and a traversal out of it is refused even though it is spelled
    // as a child of the granted prefix.
    let escape = format!("{}/../etc/passwd", dir.to_string_lossy());
    assert!(matches!(
        table.open_read_checked(&policy, &escape),
        Err(FdCapabilityError::Denied(_))
    ));
    let _ = fs::remove_file(&path);
}

#[test]
fn read_write_open_requires_both_capabilities() {
    let path = temp_file_with("rw");
    let table = FileDescriptorTable::new();
    // Read-only grant: an fd that can also write must be refused.
    let read_only = caps(CapabilityMode::Enforce, "file-read:*");
    assert!(matches!(
        table.open_read_write_checked(&read_only, &path, false),
        Err(FdCapabilityError::Denied(_))
    ));
    let both = caps(CapabilityMode::Enforce, "file-read:*;file-write:*");
    let fd = table
        .open_read_write_checked(&both, &path, false)
        .expect("both granted");
    table.close(fd).unwrap();
    let _ = fs::remove_file(&path);
}

#[test]
fn network_openers_are_gated_on_the_endpoint() {
    let table = FileDescriptorTable::new();
    let policy = caps(CapabilityMode::Enforce, "network:127.0.0.1:0-65535");
    // Wrong host — refused before any connect is attempted, so this test
    // does no I/O at all.
    assert!(matches!(
        table.open_tcp_connect_checked(&policy, "169.254.169.254:80"),
        Err(FdCapabilityError::Denied(_))
    ));
    // A UDP socket with no bind address cannot name an endpoint, so the
    // narrow grant must refuse it (fail closed).
    assert!(matches!(
        table.open_udp_checked(&policy, None),
        Err(FdCapabilityError::Denied(_))
    ));
    // A loopback listener on an ephemeral port is inside the grant.
    let fd = table
        .open_tcp_listener_checked(&policy, "127.0.0.1:0")
        .expect("granted endpoint");
    table.close(fd).unwrap();
}

#[test]
fn a_denial_maps_to_permission_denied_for_io_only_call_sites() {
    let table = FileDescriptorTable::new();
    let policy = caps(CapabilityMode::Enforce, "");
    let err = table
        .open_read_checked(&policy, "/anything")
        .expect_err("empty enforce set denies everything");
    let as_io: io::Error = err.into();
    assert_eq!(as_io.kind(), io::ErrorKind::PermissionDenied);
    assert!(as_io.to_string().contains("file-read"), "{as_io}");
}

// -----------------------------------------------------------------------
// seek/position/size/truncate on `FileRead` + `FileWrite` entries.
//
// `FileChannel`s obtained from a `FileInputStream`/`FileOutputStream` are
// backed by these two variants, and on Windows the JDK's
// `FileChannelImpl.transferToDirect` calls `position()` (=> `rw_seek`) on
// the source before every transfer. Accepting only `FileReadWrite` here
// made `ExpandWar.copy` — and so all of `TestManagerWebapp` — fail with
// `IOException: seek0: bad fd for rw_seek`.
// -----------------------------------------------------------------------

#[test]
fn rw_seek_and_position_work_on_a_read_fd() {
    let path = temp_file_with("0123456789");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(&path).unwrap();

    assert_eq!(table.rw_position(fd).unwrap(), 0);
    assert_eq!(table.rw_seek(fd, io::SeekFrom::Start(4)).unwrap(), 4);
    assert_eq!(table.rw_position(fd).unwrap(), 4);

    // The seek must be honoured by the following sequential read — i.e.
    // the BufReader's own buffer was reconciled, not left stale.
    let mut buf = [0u8; 3];
    assert_eq!(table.rw_read(fd, &mut buf).unwrap(), 3);
    assert_eq!(&buf, b"456");
    assert_eq!(table.rw_position(fd).unwrap(), 7);
    // ...and by `read_bytes`, which the `read0` native uses.
    let mut buf2 = [0u8; 3];
    assert_eq!(table.read_bytes(fd, &mut buf2).unwrap(), 3);
    assert_eq!(&buf2, b"789");

    assert_eq!(table.rw_seek(fd, io::SeekFrom::End(-2)).unwrap(), 8);
    assert_eq!(table.file_size(fd).unwrap(), 10);
    table.close(fd).unwrap();
    let _ = fs::remove_file(&path);
}

#[test]
fn rw_seek_position_and_size_work_on_a_write_fd() {
    let path = temp_path("seek_write_fd");
    let table = FileDescriptorTable::new();
    let fd = table.open_write(&path, false).unwrap();

    assert_eq!(table.rw_position(fd).unwrap(), 0);
    table.write_bytes(fd, b"abcdef").unwrap();
    assert_eq!(table.rw_position(fd).unwrap(), 6);
    assert_eq!(table.file_size(fd).unwrap(), 6);

    // Re-seek and overwrite in place; the bytes must land at offset 2.
    assert_eq!(table.rw_seek(fd, io::SeekFrom::Start(2)).unwrap(), 2);
    assert_eq!(table.rw_write(fd, b"XY").unwrap(), 2);
    assert_eq!(table.rw_position(fd).unwrap(), 4);
    table.close(fd).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"abXYef");
    let _ = fs::remove_file(&path);
}

#[test]
fn rw_set_length_truncates_a_write_fd() {
    let path = temp_path("truncate_write_fd");
    let table = FileDescriptorTable::new();
    let fd = table.open_write(&path, false).unwrap();
    table.write_bytes(fd, b"abcdefgh").unwrap();
    table.rw_set_length(fd, 3).unwrap();
    assert_eq!(table.file_size(fd).unwrap(), 3);
    table.close(fd).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"abc");
    let _ = fs::remove_file(&path);
}

#[test]
fn rw_sync_accepts_a_write_fd() {
    let path = temp_path("sync_fds");
    let table = FileDescriptorTable::new();
    let wfd = table.open_write(&path, false).unwrap();
    table.write_bytes(wfd, b"durable").unwrap();
    table.rw_sync(wfd, true).unwrap();
    table.rw_sync(wfd, false).unwrap();
    table.close(wfd).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"durable");

    // A read-only handle stays rejected — see the comment on `rw_sync`.
    let rfd = table.open_read(&path).unwrap();
    assert!(table.rw_sync(rfd, false).is_err());
    table.close(rfd).unwrap();
    let _ = fs::remove_file(&path);
}

#[test]
fn seek_family_still_rejects_non_file_fds() {
    let table = FileDescriptorTable::new();
    // stdout is not seekable: the kernel's `lseek` answer, ESPIPE (round
    // nio20 wave 3, lane L5; it was a synthetic `NotFound("bad fd for
    // rw_seek")` with no errno, which libnio's helpers could not word).
    let err = table.rw_seek(1, io::SeekFrom::Start(0)).unwrap_err();
    assert_eq!(err.raw_os_error(), not_seekable().raw_os_error(), "{err}");
    assert!(table.rw_position(1).is_err());
    assert!(table.file_size(1).is_err());
    assert!(table.rw_set_length(1, 0).is_err());
    // ...and an fd that was never opened: EBADF.
    let err = table.rw_seek(4242, io::SeekFrom::Start(0)).unwrap_err();
    assert_eq!(err.raw_os_error(), bad_fd().raw_os_error(), "{err}");
    assert!(table.is_other(4242).is_none());
    // libnio's `isOther0`: a regular file is not "other", a socket is.
    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    let rfd = table.open_read(manifest).unwrap();
    assert!(!table.is_other(rfd).unwrap().unwrap());
    table.close(rfd).unwrap();
    let (pr, pw) = table.open_pipe();
    assert!(table.is_other(pr).unwrap().unwrap());
    assert!(table.is_other(pw).unwrap().unwrap());
}

/// SBR5 lane ncall: a `RandomAccessFile` entry applies `seek(Start)` lazily
/// (one `pread`/`pwrite` instead of `lseek` + `read`/`write`). Every
/// observable cursor behaviour must equal the eager implementation's.
#[test]
fn lazy_seek_is_indistinguishable_from_an_eager_cursor() {
    use std::io::Seek;
    let path = temp_file_with("0123456789abcdef");
    let table = FileDescriptorTable::new();
    let fd = table.open_random_access(&path, true).unwrap();

    // seek + read advances the logical cursor by what was read.
    table.rw_seek(fd, io::SeekFrom::Start(4)).unwrap();
    let mut buf = [0u8; 3];
    assert_eq!(table.rw_read(fd, &mut buf).unwrap(), 3);
    assert_eq!(&buf, b"456");
    assert_eq!(table.rw_position(fd).unwrap(), 7);
    // a second read continues from there (sequential semantics).
    assert_eq!(table.rw_read(fd, &mut buf).unwrap(), 3);
    assert_eq!(&buf, b"789");

    // seek + write, then the cursor follows the write.
    table.rw_seek(fd, io::SeekFrom::Start(2)).unwrap();
    table.write_bytes(fd, b"XY").unwrap();
    assert_eq!(table.rw_position(fd).unwrap(), 4);
    table.write_bytes(fd, b"Z").unwrap();
    assert_eq!(table.rw_position(fd).unwrap(), 5);

    // relative and end-relative seeks compose with a pending position.
    table.rw_seek(fd, io::SeekFrom::Current(-5)).unwrap();
    assert_eq!(table.rw_position(fd).unwrap(), 0);
    let mut all = [0u8; 16];
    assert_eq!(table.rw_read(fd, &mut all).unwrap(), 16);
    assert_eq!(&all, b"01XYZ56789abcdef");
    assert_eq!(table.rw_seek(fd, io::SeekFrom::End(-2)).unwrap(), 14);
    let mut two = [0u8; 2];
    assert_eq!(table.rw_read(fd, &mut two).unwrap(), 2);
    assert_eq!(&two, b"ef");
    // reads past EOF return 0 and leave the cursor at EOF.
    assert_eq!(table.rw_read(fd, &mut two).unwrap(), 0);
    assert_eq!(table.rw_position(fd).unwrap(), 16);

    // seeking before the start through Current is the OS's error.
    table.rw_seek(fd, io::SeekFrom::Start(1)).unwrap();
    assert!(table.rw_seek(fd, io::SeekFrom::Current(-2)).is_err());

    // A clone (mmap / transferTo) must see the OS offset brought up to date.
    table.rw_seek(fd, io::SeekFrom::Start(9)).unwrap();
    let mut clone = table.clone_file(fd).unwrap();
    assert_eq!(clone.stream_position().unwrap(), 9);
    let mut rest = String::new();
    io::Read::read_to_string(&mut clone, &mut rest).unwrap();
    assert_eq!(rest, "9abcdef");

    // Positional reads/writes restore the logical cursor.
    table.rw_seek(fd, io::SeekFrom::Start(3)).unwrap();
    let mut p = [0u8; 2];
    assert_eq!(table.pread_at(fd, &mut p, 10).unwrap(), 2);
    assert_eq!(&p, b"ab");
    assert_eq!(table.rw_position(fd).unwrap(), 3);
    table.pwrite_at(fd, b"!!", 0).unwrap();
    assert_eq!(table.rw_position(fd).unwrap(), 3);
    assert_eq!(table.file_size(fd).unwrap(), 16);

    table.close(fd).unwrap();
    let _ = fs::remove_file(&path);
}

/// SBR5 lane ncall: only a `FileRead` over a regular file is "regular";
/// the flag dies with the fd, and a directory-less/unknown fd is not.
#[test]
fn regular_file_reads_are_recorded_and_forgotten() {
    let path = temp_file_with("abc");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(&path).unwrap();
    assert!(table.is_regular_file_read(fd));
    assert!(
        !table.is_regular_file_read(0),
        "stdin is not a regular file read"
    );
    assert!(!table.is_regular_file_read(4242));
    table.close(fd).unwrap();
    assert!(!table.is_regular_file_read(fd));
    let _ = fs::remove_file(&path);
}

#[test]
fn rw_read_and_rw_write_keep_their_direction() {
    let rpath = temp_file_with("data");
    let wpath = temp_path("direction_write");
    let table = FileDescriptorTable::new();
    let rfd = table.open_read(&rpath).unwrap();
    let wfd = table.open_write(&wpath, false).unwrap();

    assert!(table.rw_write(rfd, b"X").is_err());
    let mut buf = [0u8; 1];
    assert!(table.rw_read(wfd, &mut buf).is_err());

    table.close(rfd).unwrap();
    table.close(wfd).unwrap();
    let _ = fs::remove_file(&rpath);
    let _ = fs::remove_file(&wpath);
}
