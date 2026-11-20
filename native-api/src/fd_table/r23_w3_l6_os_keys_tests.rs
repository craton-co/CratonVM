use super::*;
use std::io::Write as _;

fn temp(content: &str) -> String {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "cratonvm_oskeys_{}_{}.txt",
        std::process::id(),
        n
    ));
    let mut f = fs::File::create(&path).unwrap();
    f.write_all(content.as_bytes()).unwrap();
    path.to_string_lossy().into_owned()
}

/// An OS key: a real OS value, never a standard stream, below the private
/// range. On Linux it is checked against `/proc/self/fd` too.
fn assert_os_key(fd: FdId, what: &str) {
    assert!(fd > 2 && fd < OS_KEY_LIMIT, "{what}: {fd:#x} is not an OS key");
    #[cfg(target_os = "linux")]
    assert!(
        std::path::Path::new(&format!("/proc/self/fd/{fd}")).exists(),
        "{what}: {fd} is not an open descriptor of this process"
    );
}

#[test]
fn os_key_of_keeps_the_standard_streams_and_the_private_range_out() {
    assert_eq!(os_key_of(None), None);
    for v in [0u64, 1, 2] {
        assert_eq!(os_key_of(Some(v)), None, "{v} is a standard stream");
    }
    assert_eq!(os_key_of(Some(3)), Some(3));
    assert_eq!(os_key_of(Some(0x1A4)), Some(0x1A4));
    assert_eq!(os_key_of(Some(u64::from(OS_KEY_LIMIT) - 1)), Some(OS_KEY_LIMIT - 1));
    assert_eq!(os_key_of(Some(u64::from(OS_KEY_LIMIT))), None);
    assert_eq!(os_key_of(Some(u64::MAX)), None);
    assert!(OS_KEY_LIMIT < NET_ID_BASE, "the private range stays below the Net ids");
}

/// The five path-opening sites and their read/write paths.
#[test]
fn every_file_site_is_keyed_by_its_os_value() {
    let table = FileDescriptorTable::new_with_os_keys(true);
    assert!(table.uses_os_keys());
    let path = temp("hello");
    let r = table.open_read(&path).unwrap();
    assert_os_key(r, "open_read");
    assert!(table.is_regular_file_read(r), "the regular-file mark moved with the key");
    let mut buf = [0u8; 5];
    assert_eq!(table.read_bytes(r, &mut buf).unwrap(), 5);
    let w = table.open_write(&path, true).unwrap();
    assert_os_key(w, "open_write");
    table.write_bytes(w, b"!").unwrap();
    let wo = table.open_write_options(&path, false, false, false).unwrap();
    assert_os_key(wo, "open_write_options");
    let rw = table.open_read_write(&path, false).unwrap();
    assert_os_key(rw, "open_read_write");
    let ra = table.open_random_access(&path, false).unwrap();
    assert_os_key(ra, "open_random_access");
    assert!(table.is_regular_file(ra));
    let keys = [r, w, wo, rw, ra];
    let distinct: std::collections::BTreeSet<_> = keys.iter().collect();
    assert_eq!(distinct.len(), keys.len(), "open descriptors are distinct: {keys:?}");
    for k in keys {
        table.close(k).unwrap();
    }
    assert!(!table.is_regular_file(ra), "close drops the mark");
    // A key the OS hands out again after a close is a fresh entry.
    let again = table.open_read(&path).unwrap();
    assert_os_key(again, "open_read after close");
    assert_eq!(table.read_bytes(again, &mut buf).unwrap(), 5);
    table.close(again).unwrap();
    let _ = fs::remove_file(&path);
}

/// A private id: above every OS key, below the `Net` ids, never reused.
fn assert_private(fd: FdId, what: &str) {
    assert!(
        (OS_KEY_LIMIT..NET_ID_BASE).contains(&fd),
        "{what}: {fd:#x} is not a private id"
    );
}

/// JIT r23 wave 4 review (`r23-w4-review-stand-in-socket-id-read-close-race`,
/// reverting wave 4b): the socket sites -- UDP (three), the TCP listener,
/// connect and accept (`insert_tcp_stream`) -- keep PRIVATE ids even with OS
/// keys on: a stand-in reader that loaded an id just before a concurrent
/// close must find nothing, never a later socket on a reused OS value.
#[test]
fn every_socket_site_keeps_a_private_id() {
    let table = FileDescriptorTable::new_with_os_keys(true);
    let u = table.open_udp(Some("127.0.0.1:0")).unwrap();
    assert_private(u, "open_udp");
    let ur = table.open_udp_reuse(Some("127.0.0.1:0")).unwrap();
    assert_private(ur, "open_udp_reuse");
    if let Ok(ud) = table.open_udp_dual_stack() {
        assert_private(ud, "open_udp_dual_stack");
        table.close(ud).unwrap();
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let c = table.open_tcp_connect(&addr).unwrap();
    assert_private(c, "open_tcp_connect");
    let (accepted, _) = listener.accept().unwrap();
    let a = table.insert_tcp_stream(accepted);
    assert_private(a, "insert_tcp_stream");
    let l = table.open_tcp_listener("127.0.0.1:0").unwrap();
    assert_private(l, "open_tcp_listener");
    for k in [u, ur, c, a, l] {
        assert_eq!(table.generation(k), Some(0), "a private id is never reused");
        table.close(k).unwrap();
        assert_eq!(table.generation(k), None, "a closed private id names nothing");
    }
    // Private ids are monotonic: a close never hands one out again.
    let again = table.open_udp(Some("127.0.0.1:0")).unwrap();
    assert!(![u, ur, c, a, l].contains(&again), "{again:#x} was reused");
    table.close(again).unwrap();
}

/// `insert_win32_file`, the merged child pipe and the child stdio pipes
/// take OS keys (wave 4: the synthetic `Process` hands out one stream per
/// pipe and its row fallback is generation-checked).
#[test]
fn the_wrapped_os_object_sites_are_keyed_by_their_os_value() {
    let table = FileDescriptorTable::new_with_os_keys(true);
    let path = temp("w");
    let f = table.insert_win32_file(fs::File::open(&path).unwrap());
    assert_os_key(f, "insert_win32_file");
    table.close(f).unwrap();
    let (reader, writer) = std::io::pipe().unwrap();
    // JIT r23 wave 4, lane L6: child pipes are OS-keyed again (the
    // synthetic `Process` now hands out one stream per pipe).
    let m = table.insert_child_merged(reader);
    assert_os_key(m, "insert_child_merged");
    drop(writer);
    table.close(m).unwrap();
    #[cfg(unix)]
    let mut cmd = {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", "cat >/dev/null; echo out; echo err 1>&2"]);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "echo out & echo err 1>&2"]);
        c
    };
    #[cfg(any(unix, windows))]
    {
        use std::process::Stdio;
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let i = table.insert_child_stdin(child.stdin.take().unwrap());
        assert_os_key(i, "insert_child_stdin");
        let o = table.insert_child_stdout(child.stdout.take().unwrap());
        assert_os_key(o, "insert_child_stdout");
        let e = table.insert_child_stderr(child.stderr.take().unwrap());
        assert_os_key(e, "insert_child_stderr");
        assert!(table.generation(o).is_some_and(|g| g > 0), "an OS-keyed pipe is stamped");
        for k in [i, o, e] {
            table.close(k).unwrap();
        }
        let _ = child.wait();
    }
    let _ = fs::remove_file(&path);
}

/// The in-memory pipe has no OS value: private ids from `OS_KEY_LIMIT`.
/// (`open_tls_connect` takes the same private arm; it needs a server.)
#[test]
fn the_in_memory_pipe_takes_private_ids() {
    let table = FileDescriptorTable::new_with_os_keys(true);
    let (r, w) = table.open_pipe();
    assert_private(r, "open_pipe read end");
    assert_private(w, "open_pipe write end");
    assert_ne!(r, w);
    table.close(r).unwrap();
    table.close(w).unwrap();
}

/// `rekey` keeps the provisional id with the switch off, without an OS
/// value, for a standard-stream value, and when the key is taken.
#[test]
fn rekey_falls_back_to_the_private_id() {
    let table = FileDescriptorTable::new_with_os_keys(true);
    let path = temp("k");
    let held = table.open_read(&path).unwrap();
    let provisional = table.alloc_fd_or_abort();
    table.entries.write().insert(
        provisional,
        Arc::new(FileEntry::PipeRead(Arc::new(Default::default()))),
    );
    assert_eq!(table.rekey(provisional, None), provisional);
    assert_eq!(table.rekey(provisional, Some(1)), provisional);
    assert_eq!(table.rekey(provisional, Some(u64::from(held))), provisional, "taken");
    table.close(provisional).unwrap();
    table.close(held).unwrap();
    let off = FileDescriptorTable::new_with_os_keys(false);
    let a = off.open_read(&path).unwrap();
    let b = off.open_read(&path).unwrap();
    assert_eq!((a, b), (3, 4), "sequential ids with the switch off");
    assert_eq!(off.generation(a), Some(0), "a sequential id is never reused");
    off.close(a).unwrap();
    off.close(b).unwrap();
    let _ = fs::remove_file(&path);
}

/// JIT r23 wave 3 review (`r23-w3-review-fd-os-keys-reuse-audit`): every
/// owner of an OS key gets its own generation, and a late close carrying
/// an earlier owner's generation is a no-op on the later owner.
#[test]
fn a_stale_generation_cannot_close_a_reused_key() {
    let table = FileDescriptorTable::new_with_os_keys(true);
    let path = temp("generation");
    let first = table.open_read(&path).unwrap();
    let g1 = table.generation(first).expect("open");
    assert_ne!(g1, 0, "an OS key carries a real generation");
    table.close(first).unwrap();
    assert_eq!(table.generation(first), None, "closed");
    // Reopen until the OS hands the same value back (it normally does at
    // once: the lowest free descriptor on Unix, a freed HANDLE slot on
    // Windows), holding the misses so they cannot be handed out again.
    let mut misses = Vec::new();
    let mut reused = None;
    for _ in 0..64 {
        let fd = table.open_read(&path).unwrap();
        if fd == first {
            reused = Some(fd);
            break;
        }
        misses.push(fd);
    }
    if let Some(fd) = reused {
        let g2 = table.generation(fd).expect("open");
        assert_ne!(g1, g2, "the reused key has a fresh generation");
        // The stale owner's late close must not touch the new owner.
        table.close_if_generation(fd, g1).unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(table.read_bytes(fd, &mut buf).unwrap(), 4, "still open");
        // The live owner's own generation closes it.
        table.close_if_generation(fd, g2).unwrap();
        assert_eq!(table.generation(fd), None);
    }
    for fd in misses {
        table.close(fd).unwrap();
    }
    let _ = fs::remove_file(&path);
}

/// `close_if_generation` on an id nobody holds, or with the live
/// generation of a never-reused id (0), behaves as `close`.
#[test]
fn close_if_generation_matches_close_for_the_live_owner() {
    let table = FileDescriptorTable::new_with_os_keys(true);
    table.close_if_generation(OS_KEY_LIMIT + 77, 0).unwrap();
    let (r, w) = table.open_pipe();
    assert_eq!(table.generation(r), Some(0));
    table.close_if_generation(r, 0).unwrap();
    assert_eq!(table.generation(r), None, "generation 0 closes a private id");
    table.close_if_generation(w, 5).unwrap();
    assert_eq!(table.generation(w), Some(0), "a wrong generation is refused");
    table.close(w).unwrap();
}
