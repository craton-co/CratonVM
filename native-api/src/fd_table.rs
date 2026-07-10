// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! File descriptor table for I/O operations.

use parking_lot::{Mutex, RwLock};
use rustc_hash::FxHashMap;
// AUDIT 2026-05-16: std::collections::HashMap is unused — fd_table uses
// rustc_hash::FxHashMap (T10.9.B).
use std::collections::VecDeque;
use std::fs;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

// ---------------------------------------------------------------------------
// FileDescriptorTable
// ---------------------------------------------------------------------------

pub type FdId = u32;

const PIPE_BUFFER_INITIAL_CAPACITY: usize = 8192;
/// Hard cap for in-memory pipes. Writers get partial progress or WouldBlock
/// instead of growing the VecDeque without bound.
const PIPE_BUFFER_CAPACITY: usize = 64 * 1024;

fn stdio_write_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Connect to `addr`, trying IPv4 candidate addresses before IPv6.
///
/// `std::net::TcpStream::connect(host:port)` resolves the host and tries each
/// returned address in resolver order until one succeeds. On Windows, an
/// unqualified `localhost` resolves to `::1` (IPv6) *first*, then `127.0.0.1`
/// — the loopback hosts-file entries are commented out by default, so the DNS
/// client returns IPv6 ahead of IPv4. When the peer (e.g. an embedded Tomcat
/// started with `-Djava.net.preferIPv4Stack=true`) listens on IPv4 only, the
/// `[::1]:port` attempt is silently dropped and blocks for ~2 seconds before
/// the stack gives up and falls back to `127.0.0.1`. That ~2s-per-connect
/// stall is what made three serial WebSocket client connects exceed a 3s
/// session-idle timeout.
///
/// Re-ordering IPv4 candidates ahead of IPv6 mirrors `preferIPv4Stack=true`
/// and makes the common loopback case connect immediately, while still
/// falling back to IPv6 for genuinely IPv6-only hosts. A literal IP address
/// resolves to a single candidate, so the sort is a no-op for it.
fn connect_prefer_ipv4(addr: &str) -> io::Result<std::net::TcpStream> {
    use std::net::ToSocketAddrs;

    let mut candidates: Vec<std::net::SocketAddr> = addr.to_socket_addrs()?.collect();
    if candidates.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("could not resolve address: {addr}"),
        ));
    }
    // Stable sort: IPv4 (key 0) before IPv6 (key 1), preserving the resolver's
    // relative order within each family.
    candidates.sort_by_key(|sa| u8::from(sa.is_ipv6()));

    let mut last_err: Option<io::Error> = None;
    for sa in &candidates {
        match std::net::TcpStream::connect(sa) {
            Ok(stream) => return Ok(stream),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::Other, format!("connect failed: {addr}"))))
}

enum FileEntry {
    Stdin(Mutex<io::Stdin>),
    Stdout(Mutex<io::Stdout>),
    Stderr(Mutex<io::Stderr>),
    FileRead(Mutex<BufReader<fs::File>>),
    FileWrite(Mutex<BufWriter<fs::File>>),
    /// Read+write access for AsynchronousFileChannel / RandomAccessFile
    FileReadWrite(Mutex<fs::File>),
    /// UDP socket for DatagramChannel
    UdpSocket(Mutex<std::net::UdpSocket>),
    /// TCP stream socket (SocketChannel)
    TcpStream(Mutex<std::net::TcpStream>),
    /// TCP listener socket (ServerSocketChannel).
    ///
    /// The `pending` queue holds connections that were `accept()`-ed off
    /// the OS backlog but not yet handed to a Java-level `tcp_accept`
    /// caller. It is kept for backward compatibility: `poll_ready` now
    /// uses a non-destructive `poll` probe and never pre-`accept()`s, so
    /// the queue normally stays empty and `tcp_accept` drains it (if a
    /// legacy/stashed entry exists) before falling back to a live
    /// `accept()`.
    TcpListener {
        listener: Mutex<std::net::TcpListener>,
        pending: Mutex<VecDeque<(std::net::TcpStream, std::net::SocketAddr)>>,
    },
    /// Read end of an in-memory pipe
    PipeRead(Arc<Mutex<VecDeque<u8>>>),
    /// Write end of an in-memory pipe
    PipeWrite(Arc<Mutex<VecDeque<u8>>>),
    /// TLS-wrapped TCP stream (SSLSocket)
    TlsStream(Mutex<native_tls::TlsStream<std::net::TcpStream>>),
    /// WP1.12 — subprocess stdout pipe (read side). Wraps the `ChildStdout`
    /// handle returned by `std::process::Command::spawn()` so the JDK's
    /// `FileInputStream.read*` natives can pull bytes through the same
    /// `fd_table().read_bytes()` API they use for files.
    ChildStdoutPipe(Mutex<std::process::ChildStdout>),
    /// WP1.12 — subprocess stderr pipe (read side).
    ChildStderrPipe(Mutex<std::process::ChildStderr>),
    /// WP1.12 — subprocess stdin pipe (write side). Bytes written go to
    /// the child process's standard input.
    ChildStdinPipe(Mutex<std::process::ChildStdin>),
    /// `ProcessBuilder.redirectErrorStream(true)` (`2>&1`): a single OS pipe
    /// whose write-end is handed to BOTH the child's stdout and stderr, so the
    /// merged output is read through one fd. getInputStream() reads this;
    /// getErrorStream() is then the empty stream.
    ChildMergedPipe(Mutex<std::io::PipeReader>),
}

/// Non-destructive socket readiness probe.
///
/// Polls the raw OS socket handle for read/write readiness **without**
/// mutating any shared socket state. Unlike a `set_nonblocking(true);
/// peek(); set_nonblocking(false)` dance, this:
///
///  * does not flip the socket's persistent blocking mode (so a
///    concurrent blocking `tcp_read` / `udp_recv` on the same fd can
///    never observe a transient non-blocking window and return a
///    spurious `WouldBlock`), and
///  * does not consume or peek datagram/stream payload.
///
/// It uses the OS `poll` (Unix) / `WSAPoll` (Windows) primitive with a
/// zero timeout, which is purely a query of kernel socket state.
///
/// Returns `(readable, writable)`. On any error the socket is reported
/// as not-readable / not-writable, matching the previous fallback
/// behaviour of `poll_ready`.
fn poll_socket_readiness<S>(sock: &S) -> (bool, bool)
where
    S: socket2_raw::AsRawHandle,
{
    socket2_raw::poll_readiness(sock.as_raw())
}

/// Thin, dependency-free wrapper over the OS `poll` / `WSAPoll`
/// readiness primitive. Kept in its own module so the platform `extern`
/// blocks and constants do not leak into the rest of `fd_table`.
mod socket2_raw {
    /// Abstraction over "give me the raw socket handle for polling".
    pub trait AsRawHandle {
        fn as_raw(&self) -> RawHandle;
    }

    #[cfg(unix)]
    pub type RawHandle = std::os::fd::RawFd;
    #[cfg(windows)]
    pub type RawHandle = std::os::windows::io::RawSocket;

    #[cfg(unix)]
    impl<T: std::os::fd::AsRawFd> AsRawHandle for T {
        fn as_raw(&self) -> RawHandle {
            self.as_raw_fd()
        }
    }
    #[cfg(windows)]
    impl<T: std::os::windows::io::AsRawSocket> AsRawHandle for T {
        fn as_raw(&self) -> RawHandle {
            self.as_raw_socket()
        }
    }

    // `nfds_t` (the `poll` count argument) is `unsigned long` on Linux
    // and the BSDs but `unsigned int` on macOS/iOS — declare it with the
    // matching width so the FFI ABI is correct on every Unix target.
    #[cfg(all(unix, any(target_os = "macos", target_os = "ios")))]
    type NfdsT = u32;
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "ios"))))]
    type NfdsT = u64;

    #[cfg(unix)]
    pub fn poll_readiness(fd: RawHandle) -> (bool, bool) {
        // struct pollfd { int fd; short events; short revents; }
        #[repr(C)]
        struct PollFd {
            fd: i32,
            events: i16,
            revents: i16,
        }
        const POLLIN: i16 = 0x0001;
        const POLLOUT: i16 = 0x0004;
        // Error/hangup conditions also make the fd "ready" — the
        // subsequent read/write will surface the actual error rather
        // than blocking, which is the readiness contract callers want.
        const POLLERR: i16 = 0x0008;
        const POLLHUP: i16 = 0x0010;
        const POLLNVAL: i16 = 0x0020;

        extern "C" {
            fn poll(fds: *mut PollFd, nfds: NfdsT, timeout: i32) -> i32;
        }

        let mut pfd = PollFd {
            fd,
            events: POLLIN | POLLOUT,
            revents: 0,
        };
        // SAFETY: `pfd` is a single, properly-initialised `pollfd`;
        // `nfds == 1` matches the one-element buffer; timeout 0 makes
        // the call return immediately without blocking.
        let rc = unsafe { poll(&mut pfd as *mut PollFd, 1 as NfdsT, 0) };
        if rc < 0 {
            return (false, false);
        }
        if pfd.revents & POLLNVAL != 0 {
            return (false, false);
        }
        let err = pfd.revents & (POLLERR | POLLHUP) != 0;
        let readable = pfd.revents & POLLIN != 0 || err;
        let writable = pfd.revents & POLLOUT != 0 || err;
        (readable, writable)
    }

    #[cfg(windows)]
    pub fn poll_readiness(socket: RawHandle) -> (bool, bool) {
        // WSAPOLLFD { SOCKET fd; SHORT events; SHORT revents; }
        #[repr(C)]
        struct WsaPollFd {
            fd: usize,
            events: i16,
            revents: i16,
        }
        const POLLRDNORM: i16 = 0x0100;
        const POLLWRNORM: i16 = 0x0010;
        const POLLERR: i16 = 0x0001;
        const POLLHUP: i16 = 0x0002;
        const POLLNVAL: i16 = 0x0004;

        #[link(name = "ws2_32")]
        extern "system" {
            fn WSAPoll(fds: *mut WsaPollFd, nfds: u32, timeout: i32) -> i32;
        }

        let mut pfd = WsaPollFd {
            fd: socket as usize,
            events: POLLRDNORM | POLLWRNORM,
            revents: 0,
        };
        // SAFETY: single properly-initialised WSAPOLLFD, `nfds == 1`
        // matches the buffer length, timeout 0 returns immediately.
        let rc = unsafe { WSAPoll(&mut pfd as *mut WsaPollFd, 1, 0) };
        if rc < 0 {
            return (false, false);
        }
        if pfd.revents & POLLNVAL != 0 {
            return (false, false);
        }
        let err = pfd.revents & (POLLERR | POLLHUP) != 0;
        let readable = pfd.revents & POLLRDNORM != 0 || err;
        let writable = pfd.revents & POLLWRNORM != 0 || err;
        (readable, writable)
    }
}

/// Thread-safe registry of open file handles, keyed by integer file descriptors.
///
/// Pre-registers fd 0/1/2 for stdin/stdout/stderr.
/// T10.9.B: FxHashMap — fd IDs are internal u32 counters.
///
/// AUDIT 2026-05-16 (B3/B4 native-api): entries are wrapped in `Arc` so a
/// caller can obtain a handle to a `FileEntry`, drop the table-level
/// `RwLock` read guard, and *then* perform the (possibly blocking) syscall
/// on the entry's inner `Mutex`. Holding the table read guard across a
/// syscall would serialize every `close()` / `open_*()` / `insert_*()`
/// against the longest-running I/O on any fd in the VM — a blocking
/// stdin or TCP read could freeze fd-table mutations indefinitely.
pub struct FileDescriptorTable {
    entries: RwLock<FxHashMap<FdId, Arc<FileEntry>>>,
    next_fd: AtomicU32,
}

impl FileDescriptorTable {
    pub fn new() -> Self {
        let mut entries: FxHashMap<FdId, Arc<FileEntry>> = FxHashMap::default();
        entries.insert(0, Arc::new(FileEntry::Stdin(Mutex::new(io::stdin()))));
        entries.insert(1, Arc::new(FileEntry::Stdout(Mutex::new(io::stdout()))));
        entries.insert(2, Arc::new(FileEntry::Stderr(Mutex::new(io::stderr()))));
        Self {
            entries: RwLock::new(entries),
            next_fd: AtomicU32::new(3),
        }
    }

    /// AUDIT 2026-05-16 (B3/B4): clone the `Arc<FileEntry>` for `fd` and
    /// drop the table-level `RwLock` read guard before the caller starts
    /// any blocking I/O on the entry. Returns `None` if the fd is absent.
    fn get_entry(&self, fd: FdId) -> Option<Arc<FileEntry>> {
        self.entries.read().get(&fd).cloned()
    }

    /// Allocate a fresh fd from the counter, guarding against wraparound.
    ///
    /// `Result`-returning openers (`open_read`, `open_write`, …) check the
    /// `u32::MAX - 16` ceiling themselves and surface an `io::Error`. The
    /// infallible `insert_*` / `open_pipe` paths cannot return an error, so
    /// they call this helper: on a (practically unreachable, ~4-billion-fd)
    /// overflow it aborts the process rather than letting the counter wrap
    /// and alias the reserved stdin/stdout/stderr fds (0/1/2) — silent fd
    /// aliasing would corrupt unrelated streams. `abort` is used instead of
    /// `panic!` so the failure cannot unwind through JIT/native frames.
    fn alloc_fd_or_abort(&self) -> FdId {
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            eprintln!("fatal: file descriptor counter overflow — aborting to avoid aliasing stdin/stdout/stderr");
            std::process::abort();
        }
        fd
    }

    /// Open a file for reading. Returns the fd_id.
    ///
    /// # Security
    ///
    /// `path` is passed **verbatim** to the OS with no path-traversal or
    /// sandbox check performed here. When the value originates from
    /// Java-controlled input, the caller (the VM / native-io sandbox layer)
    /// MUST sanitize it for path traversal before calling — this function
    /// deliberately does not enforce that, to avoid duplicating (and
    /// potentially conflicting with) the VM's own sandbox policy.
    pub fn open_read(&self, path: &str) -> Result<FdId, io::Error> {
        // Reserve fd first, before opening the file.
        // fds only need to be unique, not contiguous — on overflow we
        // simply fail without rolling the counter back (a `fetch_sub`
        // rollback would be racy and pointless).
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            return Err(io::Error::other("file descriptor limit exceeded"));
        }
        let file = fs::File::open(path)?;
        let reader = BufReader::new(file);
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::FileRead(Mutex::new(reader))));
        Ok(fd)
    }

    /// Open a file for writing (optionally appending). Returns the fd_id.
    ///
    /// # Security
    ///
    /// `path` is passed **verbatim** to the OS with no path-traversal or
    /// sandbox check performed here. When the value originates from
    /// Java-controlled input, the caller (the VM / native-io sandbox layer)
    /// MUST sanitize it for path traversal before calling — this function
    /// deliberately does not enforce that, to avoid duplicating (and
    /// potentially conflicting with) the VM's own sandbox policy.
    pub fn open_write(&self, path: &str, append: bool) -> Result<FdId, io::Error> {
        // Reserve fd first, before opening the file.
        // fds only need to be unique, not contiguous — on overflow we
        // simply fail without rolling the counter back (a `fetch_sub`
        // rollback would be racy and pointless).
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            return Err(io::Error::other("file descriptor limit exceeded"));
        }
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(!append)
            .append(append)
            .open(path)?;
        let writer = BufWriter::new(file);
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::FileWrite(Mutex::new(writer))));
        Ok(fd)
    }

    /// Read a single byte. Returns 0-255 or -1 at EOF.
    pub fn read_byte(&self, fd: FdId) -> Result<i32, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd"))?;
        match &*entry {
            FileEntry::Stdin(stdin) => {
                let mut buf = [0u8; 1];
                let n = stdin.lock().read(&mut buf)?;
                Ok(if n == 0 { -1 } else { buf[0] as i32 })
            }
            FileEntry::FileRead(reader) => {
                let mut buf = [0u8; 1];
                let n = reader.lock().read(&mut buf)?;
                Ok(if n == 0 { -1 } else { buf[0] as i32 })
            }
            // WP1.12 — subprocess stdout/stderr read.
            FileEntry::ChildStdoutPipe(p) => {
                let mut buf = [0u8; 1];
                let n = p.lock().read(&mut buf)?;
                Ok(if n == 0 { -1 } else { buf[0] as i32 })
            }
            FileEntry::ChildStderrPipe(p) => {
                let mut buf = [0u8; 1];
                let n = p.lock().read(&mut buf)?;
                Ok(if n == 0 { -1 } else { buf[0] as i32 })
            }
            FileEntry::ChildMergedPipe(p) => {
                let mut buf = [0u8; 1];
                let n = p.lock().read(&mut buf)?;
                Ok(if n == 0 { -1 } else { buf[0] as i32 })
            }
            FileEntry::FileReadWrite(file) => {
                let mut buf = [0u8; 1];
                let n = file.lock().read(&mut buf)?;
                Ok(if n == 0 { -1 } else { buf[0] as i32 })
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd")),
        }
    }

    /// Read up to `len` bytes into `buf`. Returns count read, or 0 at EOF.
    pub fn read_bytes(&self, fd: FdId, buf: &mut [u8]) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd"))?;
        match &*entry {
            FileEntry::Stdin(stdin) => {
                let n = stdin.lock().read(buf)?;
                Ok(n)
            }
            FileEntry::FileRead(reader) => reader.lock().read(buf),
            // WP1.12 — subprocess stdout/stderr bulk read.
            FileEntry::ChildStdoutPipe(p) => p.lock().read(buf),
            FileEntry::ChildStderrPipe(p) => p.lock().read(buf),
            FileEntry::ChildMergedPipe(p) => p.lock().read(buf),
            // Read+write file (FileChannel via newFileChannel /
            // RandomAccessFile) — the real `FileDispatcherImpl.read0` path
            // reaches here. Read at the current cursor, matching `rw_read`.
            FileEntry::FileReadWrite(file) => file.lock().read(buf),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd")),
        }
    }

    /// Read one line (for BufferedReader). Returns None at EOF.
    /// Read a single line from the file descriptor, recognizing any of
    /// the three line terminators `\n`, `\r\n`, or a bare `\r` — matching
    /// `java.io.BufferedReader.readLine` exactly (JDK 25 javadoc:
    /// "A line is considered to be terminated by any one of a line feed
    /// ('\n'), a carriage return ('\r'), a carriage return followed
    /// immediately by a line feed, or by reaching the end-of-file (EOF)").
    ///
    /// Previously this delegated to `std::io::BufRead::read_line` which
    /// only terminates on `\n`, so a file written on classic Mac OS
    /// (`\r`-only line endings) would read as a single gigantic line.
    /// T2.4.14 asked for byte-level correctness; the new implementation
    /// reads one byte at a time from the underlying stream and stops
    /// at the first terminator, consuming `\n` after `\r` if present to
    /// avoid leaking it into the next call.
    ///
    /// Returns `Ok(None)` on EOF (no bytes read), `Ok(Some(line))`
    /// otherwise. The trailing terminator is *not* included in the
    /// returned string. I/O errors propagate as `Err`.
    pub fn read_line(&self, fd: FdId) -> Result<Option<String>, io::Error> {
        // Helper that reads a single byte from a `BufRead` source and
        // returns `None` at EOF. We cannot use `read_exact([u8; 1])`
        // because EOF is a normal termination, not an error.
        fn read_one<R: io::BufRead>(r: &mut R) -> io::Result<Option<u8>> {
            let mut buf = [0u8; 1];
            match r.read(&mut buf)? {
                0 => Ok(None),
                _ => Ok(Some(buf[0])),
            }
        }
        // Loop that reads bytes from a generic `BufRead` until the
        // first line terminator. After seeing `\r`, peek the next
        // byte and consume it iff it is `\n` (so `\r\n` stays
        // atomic). Otherwise fall through.
        fn read_line_inner<R: io::BufRead>(r: &mut R) -> io::Result<Option<String>> {
            let mut bytes: Vec<u8> = Vec::new();
            loop {
                match read_one(r)? {
                    None => {
                        if bytes.is_empty() {
                            return Ok(None);
                        } else {
                            break;
                        }
                    }
                    Some(b'\n') => break,
                    Some(b'\r') => {
                        // Peek the next byte: if it's \n, consume it.
                        let peek = r.fill_buf()?;
                        if !peek.is_empty() && peek[0] == b'\n' {
                            r.consume(1);
                        }
                        break;
                    }
                    Some(b) => bytes.push(b),
                }
            }
            // Decode as UTF-8 lossily — matching JDK behavior where a
            // reader atop a charset will have already transcoded, but
            // our line reader operates at the byte level.
            Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
        }

        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd"))?;
        match &*entry {
            FileEntry::Stdin(stdin) => {
                // `io::Stdin` itself is not `BufRead`, but `StdinLock`
                // — obtained via `Stdin::lock()` — is. The outer
                // parking_lot `Mutex` guard lets us hold the
                // `StdinLock` exclusively while we read one line.
                let guard = stdin.lock();
                let mut stdin_lock = guard.lock();
                read_line_inner(&mut stdin_lock)
            }
            FileEntry::FileRead(reader) => {
                // `BufReader<File>` implements `BufRead`; the
                // parking_lot guard defers directly.
                let mut guard = reader.lock();
                read_line_inner(&mut *guard)
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd")),
        }
    }

    /// Write a single byte.
    pub fn write_byte(&self, fd: FdId, b: u8) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd"))?;
        match &*entry {
            FileEntry::Stdout(stdout) => {
                let _stdio_guard = stdio_write_lock().lock();
                stdout.lock().write_all(&[b])?;
                Ok(())
            }
            FileEntry::Stderr(stderr) => {
                let _stdio_guard = stdio_write_lock().lock();
                stderr.lock().write_all(&[b])?;
                Ok(())
            }
            FileEntry::FileWrite(writer) => {
                writer.lock().write_all(&[b])?;
                Ok(())
            }
            // WP1.12 — write to subprocess stdin.
            FileEntry::ChildStdinPipe(p) => {
                p.lock().write_all(&[b])?;
                Ok(())
            }
            FileEntry::FileReadWrite(file) => {
                file.lock().write_all(&[b])?;
                Ok(())
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd")),
        }
    }

    /// Write bytes from a slice.
    pub fn write_bytes(&self, fd: FdId, data: &[u8]) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd"))?;
        match &*entry {
            FileEntry::Stdout(stdout) => {
                let _stdio_guard = stdio_write_lock().lock();
                stdout.lock().write_all(data)?;
                Ok(())
            }
            FileEntry::Stderr(stderr) => {
                let _stdio_guard = stdio_write_lock().lock();
                stderr.lock().write_all(data)?;
                Ok(())
            }
            FileEntry::FileWrite(writer) => {
                // Java's `FileOutputStream.write` is UNBUFFERED — each write
                // reaches the OS immediately and is visible to a concurrent
                // reader on a separate handle. Our `BufWriter` would otherwise
                // hold the bytes in userspace, so a second open of the same
                // path reads an empty/partial file (DaCapo luindex's
                // `FileDigest` re-reads `stdout.log` without first flushing the
                // writer → empty-input SHA-1, digest validation fails). Flush
                // after the write to restore the immediate-visibility
                // contract; apps that want batching use `BufferedOutputStream`
                // (Java-side), which hands us already-coalesced chunks — so
                // this matches HotSpot, whose FOS issues a `write()` syscall
                // per call too.
                let mut w = writer.lock();
                w.write_all(data)?;
                w.flush()?;
                Ok(())
            }
            // WP1.12 — bulk write to subprocess stdin.
            FileEntry::ChildStdinPipe(p) => {
                p.lock().write_all(data)?;
                Ok(())
            }
            // A read+write file (FileChannel via newFileChannel /
            // RandomAccessFile) — the real `FileDispatcherImpl.write0` path
            // reaches here. Write at the file's current cursor (unbuffered
            // fs::File), matching `rw_write`. Without this arm the generic
            // write path fell to "bad fd" even though the fd is valid.
            FileEntry::FileReadWrite(file) => {
                file.lock().write_all(data)?;
                Ok(())
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd")),
        }
    }

    /// Write a UTF-8 string.
    pub fn write_string(&self, fd: FdId, text: &str) -> Result<(), io::Error> {
        self.write_bytes(fd, text.as_bytes())
    }

    /// Flush buffered output.
    pub fn flush(&self, fd: FdId) -> Result<(), io::Error> {
        // No fd present is a no-op (consistent with previous behavior
        // for non-writable / nonexistent fds).
        let Some(entry) = self.get_entry(fd) else {
            return Ok(());
        };
        match &*entry {
            FileEntry::Stdout(stdout) => {
                let _stdio_guard = stdio_write_lock().lock();
                stdout.lock().flush()?;
                Ok(())
            }
            FileEntry::Stderr(stderr) => {
                let _stdio_guard = stdio_write_lock().lock();
                stderr.lock().flush()?;
                Ok(())
            }
            FileEntry::FileWrite(writer) => {
                writer.lock().flush()?;
                Ok(())
            }
            // WP1.12 — flushing ChildStdin pushes buffered bytes into the
            // subprocess's stdin immediately. Without this, the child may
            // block waiting for data that's sitting in our userspace buffer.
            FileEntry::ChildStdinPipe(p) => {
                p.lock().flush()?;
                Ok(())
            }
            _ => Ok(()), // no-op for non-writable fds
        }
    }

    /// Close a file descriptor. Returns an error if flushing a writer fails.
    ///
    /// AUDIT 2026-05-16 (B3/B4 + B7): the entry is removed from the table
    /// *first*, releasing the write guard before any blocking I/O (flush)
    /// runs. The previous implementation held the write guard across
    /// `writer.lock().flush()?` — a slow flush would block every other
    /// fd-table mutation VM-wide. The previous code also leaked the
    /// entry on flush error: it propagated the `?` while the entry
    /// was still in the table, so the caller could neither retry nor
    /// release the resource. Now we always remove the entry on close
    /// and flush the dropped copy out-of-band, returning the flush
    /// error if any.
    pub fn close(&self, fd: FdId) -> Result<(), io::Error> {
        // Don't close stdin/stdout/stderr
        if fd < 3 {
            return Ok(());
        }
        // Remove first (under the write guard), then drop the guard
        // before performing any blocking I/O on the removed entry.
        let removed = {
            let mut entries = self.entries.write();
            entries.remove(&fd)
        };
        let Some(entry) = removed else {
            return Ok(());
        };
        // Flush writer-style entries out of the table lock. We can't
        // move out of an Arc (other clones could still exist
        // theoretically — though in practice they shouldn't), so we
        // flush through the inner Mutex on the existing Arc and let
        // `Drop` close the OS handle when the Arc count reaches zero.
        match &*entry {
            FileEntry::FileWrite(writer) => writer.lock().flush(),
            FileEntry::ChildStdinPipe(p) => p.lock().flush(),
            _ => Ok(()),
        }
    }

    /// Estimate available bytes (best-effort).
    pub fn available(&self, fd: FdId) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd"))?;
        match &*entry {
            FileEntry::FileRead(reader) => {
                let mut buf = reader.lock();
                let buffered = buf.buffer().len();
                // Also account for bytes remaining in the underlying file
                // beyond what's already buffered.
                let file_remaining = {
                    use std::io::Seek;
                    let inner = buf.get_mut();
                    let pos = inner.stream_position().unwrap_or(0);
                    let end = inner.seek(std::io::SeekFrom::End(0)).unwrap_or(0);
                    // Seek back to where we were
                    let _ = inner.seek(std::io::SeekFrom::Start(pos));
                    end.saturating_sub(pos) as usize
                };
                Ok(buffered + file_remaining)
            }
            FileEntry::Stdin(_) => Ok(0),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd")),
        }
    }

    // -----------------------------------------------------------------------
    // Phase 92: AsynchronousFileChannel, DatagramChannel support
    // -----------------------------------------------------------------------

    /// Open a file for read+write access (used by AsynchronousFileChannel).
    ///
    /// # Security
    ///
    /// `path` is passed **verbatim** to the OS with no path-traversal or
    /// sandbox check performed here. When the value originates from
    /// Java-controlled input, the caller (the VM / native-io sandbox layer)
    /// MUST sanitize it for path traversal before calling — this function
    /// deliberately does not enforce that, to avoid duplicating (and
    /// potentially conflicting with) the VM's own sandbox policy.
    pub fn open_read_write(&self, path: &str, create: bool) -> Result<FdId, io::Error> {
        // fds only need to be unique, not contiguous — on overflow we
        // simply fail without rolling the counter back (a `fetch_sub`
        // rollback would be racy and pointless).
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            return Err(io::Error::other("file descriptor limit exceeded"));
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .open(path)?;
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::FileReadWrite(Mutex::new(file))));
        Ok(fd)
    }

    /// Open a file for `java.io.RandomAccessFile`. Always produces a
    /// `FileReadWrite` entry so the RAF read/write/seek natives **and** any
    /// `FileChannel` derived from the RAF (`raf.getChannel()`) resolve to the
    /// **same** fd — they share one `FileDescriptor`, and HotSpot shares one
    /// OS fd between them. `write == false` mirrors mode `"r"` (open the file
    /// read-only: do not create it, and let a stray write fail at the OS
    /// level, matching a read-only RAF). `write == true` mirrors `"rw"`/
    /// `"rws"`/`"rwd"` (read+write, create if absent). The caller emulates the
    /// `"rws"`/`"rwd"` per-write sync via [`Self::rw_sync`].
    ///
    /// # Security
    ///
    /// `path` is passed **verbatim** to the OS with no path-traversal or
    /// sandbox check performed here. When the value originates from
    /// Java-controlled input, the caller (the VM / native-io sandbox layer)
    /// MUST sanitize it for path traversal before calling.
    pub fn open_random_access(&self, path: &str, write: bool) -> Result<FdId, io::Error> {
        // fds only need to be unique, not contiguous — on overflow we
        // simply fail without rolling the counter back (a `fetch_sub`
        // rollback would be racy and pointless).
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            return Err(io::Error::other("file descriptor limit exceeded"));
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(write)
            .create(write)
            .open(path)?;
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::FileReadWrite(Mutex::new(file))));
        Ok(fd)
    }

    /// Read bytes from a file at a specific position (pread).
    /// Does not change the file's current position.
    pub fn pread_at(&self, fd: FdId, buf: &mut [u8], position: u64) -> Result<usize, io::Error> {
        use io::{Read, Seek, SeekFrom};
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for pread"))?;
        match &*entry {
            FileEntry::FileReadWrite(file) => {
                let mut f = file.lock();
                let saved = f.stream_position()?;
                f.seek(SeekFrom::Start(position))?;
                let n = f.read(buf)?;
                f.seek(SeekFrom::Start(saved))?;
                Ok(n)
            }
            FileEntry::FileRead(reader) => {
                // Seek the `BufReader` itself (not the file behind its
                // back). `BufReader`'s `Seek` impl reconciles or discards
                // its internal buffer, and `stream_position()` returns
                // the logical position accounting for buffered-but-
                // unconsumed bytes — so saving and restoring it leaves a
                // subsequent sequential `read()` returning the correct
                // bytes. Seeking the inner file directly would leave the
                // buffer stale and corrupt the next sequential read.
                let mut r = reader.lock();
                let saved = r.stream_position()?;
                r.seek(SeekFrom::Start(position))?;
                let n = r.read(buf)?;
                r.seek(SeekFrom::Start(saved))?;
                Ok(n)
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for pread")),
        }
    }

    /// Write bytes to a file at a specific position (pwrite).
    /// Does not change the file's current position.
    pub fn pwrite_at(&self, fd: FdId, data: &[u8], position: u64) -> Result<usize, io::Error> {
        use io::{Seek, SeekFrom, Write};
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for pwrite"))?;
        match &*entry {
            FileEntry::FileReadWrite(file) => {
                let mut f = file.lock();
                let saved = f.stream_position()?;
                f.seek(SeekFrom::Start(position))?;
                f.write_all(data)?;
                f.seek(SeekFrom::Start(saved))?;
                Ok(data.len())
            }
            FileEntry::FileWrite(writer) => {
                // Seek the `BufWriter` itself, not the file behind its
                // back. `BufWriter`'s `Seek` impl flushes any pending
                // buffered bytes to their original offset *before*
                // moving the cursor — so the positional write below
                // cannot interleave with stale buffer contents, and the
                // restore-seek leaves a subsequent sequential write
                // appending at the correct offset. Seeking the inner
                // file directly would strand un-flushed bytes and write
                // them at the wrong offset on the next flush.
                let mut w = writer.lock();
                let saved = w.stream_position()?;
                w.seek(SeekFrom::Start(position))?;
                w.write_all(data)?;
                w.flush()?;
                w.seek(SeekFrom::Start(saved))?;
                Ok(data.len())
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for pwrite")),
        }
    }

    /// Sequential read from a FileReadWrite file (advances the cursor).
    pub fn rw_read(&self, fd: FdId, buf: &mut [u8]) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for rw_read"))?;
        match &*entry {
            FileEntry::FileReadWrite(file) => {
                let mut f = file.lock();
                f.read(buf)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for rw_read",
            )),
        }
    }

    /// Sequential write to a FileReadWrite file (advances the cursor).
    pub fn rw_write(&self, fd: FdId, data: &[u8]) -> Result<usize, io::Error> {
        use io::Write;
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for rw_write"))?;
        match &*entry {
            FileEntry::FileReadWrite(file) => {
                let mut f = file.lock();
                f.write(data)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for rw_write",
            )),
        }
    }

    /// Seek in a FileReadWrite file. Returns new position.
    pub fn rw_seek(&self, fd: FdId, pos: io::SeekFrom) -> Result<u64, io::Error> {
        use io::Seek;
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for rw_seek"))?;
        match &*entry {
            FileEntry::FileReadWrite(file) => {
                let mut f = file.lock();
                f.seek(pos)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for rw_seek",
            )),
        }
    }

    /// Get the current position in a FileReadWrite file.
    pub fn rw_position(&self, fd: FdId) -> Result<u64, io::Error> {
        use io::Seek;
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for rw_position"))?;
        match &*entry {
            FileEntry::FileReadWrite(file) => {
                let mut f = file.lock();
                f.stream_position()
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for rw_position",
            )),
        }
    }

    /// Clone the underlying `std::fs::File` for any file-backed entry.
    ///
    /// The returned handle refers to the same kernel file but has an
    /// independent seek cursor, which is what `mmap` / `MapViewOfFile`
    /// require. Returns an error if the fd is not backed by a real file.
    pub fn clone_file(&self, fd: FdId) -> Result<fs::File, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for clone_file"))?;
        match &*entry {
            FileEntry::FileReadWrite(file) => file.lock().try_clone(),
            FileEntry::FileRead(reader) => reader.lock().get_ref().try_clone(),
            FileEntry::FileWrite(writer) => {
                // BufWriter::get_ref() borrows; flush first to not lose data.
                let mut w = writer.lock();
                w.flush().ok();
                w.get_ref().try_clone()
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for clone_file",
            )),
        }
    }

    /// Set the length of a FileReadWrite file (truncate or extend).
    pub fn rw_set_length(&self, fd: FdId, len: u64) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for rw_set_length"))?;
        match &*entry {
            FileEntry::FileReadWrite(file) => {
                let f = file.lock();
                f.set_len(len)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for rw_set_length",
            )),
        }
    }

    /// Flush a FileReadWrite file's contents to stable storage. Used by
    /// `RandomAccessFile` modes `"rws"` (`data_only == false` → `sync_all`,
    /// data + metadata) and `"rwd"` (`data_only == true` → `sync_data`, data
    /// only) which require every write to reach durable storage before the
    /// call returns.
    pub fn rw_sync(&self, fd: FdId, data_only: bool) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for rw_sync"))?;
        match &*entry {
            FileEntry::FileReadWrite(file) => {
                let f = file.lock();
                if data_only {
                    f.sync_data()
                } else {
                    f.sync_all()
                }
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for rw_sync",
            )),
        }
    }

    /// Get the size of a file by fd.
    pub fn file_size(&self, fd: FdId) -> Result<u64, io::Error> {
        use io::{Seek, SeekFrom};
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for size"))?;
        match &*entry {
            FileEntry::FileReadWrite(file) => {
                let mut f = file.lock();
                let saved = f.stream_position()?;
                let size = f.seek(SeekFrom::End(0))?;
                f.seek(SeekFrom::Start(saved))?;
                Ok(size)
            }
            FileEntry::FileRead(reader) => {
                let mut r = reader.lock();
                let inner = r.get_mut();
                let saved = inner.stream_position()?;
                let size = inner.seek(SeekFrom::End(0))?;
                inner.seek(SeekFrom::Start(saved))?;
                Ok(size)
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for size")),
        }
    }

    /// Open a UDP socket. Returns the fd_id.
    ///
    /// # Security
    ///
    /// `bind_addr` is passed **verbatim** to the OS with no sandbox/SSRF
    /// check performed here. When the value originates from Java-controlled
    /// input, the caller (the VM / native-io sandbox layer) MUST sanitize it
    /// before calling — this function deliberately does not enforce that, to
    /// avoid duplicating (and potentially conflicting with) the VM's own
    /// sandbox policy.
    pub fn open_udp(&self, bind_addr: Option<&str>) -> Result<FdId, io::Error> {
        // fds only need to be unique, not contiguous — on overflow we
        // simply fail without rolling the counter back (a `fetch_sub`
        // rollback would be racy and pointless).
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            return Err(io::Error::other("file descriptor limit exceeded"));
        }
        let addr = bind_addr.unwrap_or("0.0.0.0:0");
        let socket = std::net::UdpSocket::bind(addr)?;
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::UdpSocket(Mutex::new(socket))));
        Ok(fd)
    }

    /// Open a UDP socket with `SO_REUSEADDR` set **before** bind. Returns the fd_id.
    ///
    /// `java.net.MulticastSocket` enables `SO_REUSEADDR` prior to binding so that
    /// multiple receivers (and repeated bind/close cycles within one process)
    /// can share a multicast group port. Plain `open_udp` binds via
    /// `std::net::UdpSocket::bind`, which does not set the option — on Windows
    /// that surfaces as `WSAEADDRINUSE` (os error 10048) when a recently-closed
    /// or concurrently-held port is re-bound. This variant mirrors the JDK
    /// semantics using `socket2` (create → set_reuse_address → bind).
    ///
    /// # Security
    ///
    /// Same `bind_addr` caveat as [`open_udp`]: the address is passed verbatim
    /// to the OS with no sandbox/SSRF check here.
    pub fn open_udp_reuse(&self, bind_addr: Option<&str>) -> Result<FdId, io::Error> {
        use std::net::ToSocketAddrs;
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            return Err(io::Error::other("file descriptor limit exceeded"));
        }
        let addr_str = bind_addr.unwrap_or("0.0.0.0:0");
        let sock_addr = addr_str
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addr resolved"))?;
        let domain = if sock_addr.is_ipv4() {
            socket2::Domain::IPV4
        } else {
            socket2::Domain::IPV6
        };
        let socket =
            socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
        socket.set_reuse_address(true)?;
        socket.bind(&sock_addr.into())?;
        let udp: std::net::UdpSocket = socket.into();
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::UdpSocket(Mutex::new(udp))));
        Ok(fd)
    }

    /// Send UDP datagram to a target address. Returns bytes sent.
    pub fn udp_send(&self, fd: FdId, data: &[u8], target: &str) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp send"))?;
        match &*entry {
            FileEntry::UdpSocket(sock) => {
                let s = sock.lock();
                s.send_to(data, target)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for udp send",
            )),
        }
    }

    /// Receive a UDP datagram. Returns (bytes_read, source_addr).
    pub fn udp_recv(&self, fd: FdId, buf: &mut [u8]) -> Result<(usize, String), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp recv"))?;
        match &*entry {
            FileEntry::UdpSocket(sock) => {
                let s = sock.lock();
                let (n, addr) = s.recv_from(buf)?;
                Ok((n, addr.to_string()))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for udp recv",
            )),
        }
    }

    /// Set non-blocking mode on a UDP socket.
    pub fn udp_set_nonblocking(&self, fd: FdId, nonblocking: bool) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(sock) => sock.lock().set_nonblocking(nonblocking),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Get the local address of a UDP socket.
    pub fn udp_local_addr(&self, fd: FdId) -> Result<String, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(sock) => Ok(sock.lock().local_addr()?.to_string()),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Connect a TCP stream to a remote address. Returns the fd_id.
    ///
    /// # Security
    ///
    /// `addr` is passed **verbatim** to the OS with no sandbox/SSRF check
    /// performed here. When the value originates from Java-controlled input,
    /// the caller (the VM / native-io sandbox layer) MUST sanitize it before
    /// calling — this function deliberately does not enforce that, to avoid
    /// duplicating (and potentially conflicting with) the VM's own sandbox
    /// policy.
    pub fn open_tcp_connect(&self, addr: &str) -> Result<FdId, io::Error> {
        // fds only need to be unique, not contiguous — on overflow we
        // simply fail without rolling the counter back (a `fetch_sub`
        // rollback would be racy and pointless).
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            return Err(io::Error::other("file descriptor limit exceeded"));
        }
        let stream = connect_prefer_ipv4(addr)?;
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::TcpStream(Mutex::new(stream))));
        Ok(fd)
    }

    /// Wrap an existing TcpStream (e.g. from accept). Returns the fd_id.
    pub fn insert_tcp_stream(&self, stream: std::net::TcpStream) -> FdId {
        let fd = self.alloc_fd_or_abort();
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::TcpStream(Mutex::new(stream))));
        fd
    }

    /// WP1.12 — wrap a subprocess's `ChildStdout` pipe and return the fd_id.
    /// The JDK-side `FileInputStream` wrapping this fd gets bytes from the
    /// child process's standard output.
    pub fn insert_child_stdout(&self, stream: std::process::ChildStdout) -> FdId {
        let fd = self.alloc_fd_or_abort();
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::ChildStdoutPipe(Mutex::new(stream))));
        fd
    }

    /// WP1.12 — wrap a subprocess's `ChildStderr` pipe and return the fd_id.
    pub fn insert_child_stderr(&self, stream: std::process::ChildStderr) -> FdId {
        let fd = self.alloc_fd_or_abort();
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::ChildStderrPipe(Mutex::new(stream))));
        fd
    }

    /// `redirectErrorStream(true)`: wrap the read-end of the single pipe that
    /// the child's stdout AND stderr both write to. Read through the same
    /// `read_bytes`/`read_byte` API as the separate pipes.
    pub fn insert_child_merged(&self, reader: std::io::PipeReader) -> FdId {
        let fd = self.alloc_fd_or_abort();
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::ChildMergedPipe(Mutex::new(reader))));
        fd
    }

    /// WP1.12 — wrap a subprocess's `ChildStdin` pipe and return the fd_id.
    /// Writing bytes to this fd pipes them into the child's standard input.
    pub fn insert_child_stdin(&self, stream: std::process::ChildStdin) -> FdId {
        let fd = self.alloc_fd_or_abort();
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::ChildStdinPipe(Mutex::new(stream))));
        fd
    }

    /// Open a TCP listener bound to the given address. Returns the fd_id.
    ///
    /// # Security
    ///
    /// `addr` is passed **verbatim** to the OS with no sandbox check
    /// performed here. When the value originates from Java-controlled input,
    /// the caller (the VM / native-io sandbox layer) MUST sanitize it before
    /// calling — this function deliberately does not enforce that, to avoid
    /// duplicating (and potentially conflicting with) the VM's own sandbox
    /// policy.
    pub fn open_tcp_listener(&self, addr: &str) -> Result<FdId, io::Error> {
        // fds only need to be unique, not contiguous — on overflow we
        // simply fail without rolling the counter back.
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            return Err(io::Error::other("file descriptor limit exceeded"));
        }
        let listener = std::net::TcpListener::bind(addr)?;
        self.entries.write().insert(
            fd,
            Arc::new(FileEntry::TcpListener {
                listener: Mutex::new(listener),
                pending: Mutex::new(VecDeque::new()),
            }),
        );
        Ok(fd)
    }

    /// Accept a connection on a TCP listener. Returns (new_stream_fd, remote_addr).
    pub fn tcp_accept(&self, fd: FdId) -> Result<(FdId, String), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp accept"))?;
        match &*entry {
            FileEntry::TcpListener { listener, pending } => {
                // Drain any connection that `poll_ready` already accepted
                // off the OS backlog before touching the listener itself.
                let (stream, addr) = match pending.lock().pop_front() {
                    Some(conn) => conn,
                    None => listener.lock().accept()?,
                };
                let new_fd = self.insert_tcp_stream(stream);
                Ok((new_fd, addr.to_string()))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for tcp accept",
            )),
        }
    }

    /// Read from a TCP stream. Returns bytes read.
    pub fn tcp_read(&self, fd: FdId, buf: &mut [u8]) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp read"))?;
        match &*entry {
            FileEntry::TcpStream(stream) => {
                let mut s = stream.lock();
                s.read(buf)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for tcp read",
            )),
        }
    }

    /// Duplicate the underlying TCP socket for `fd`, returning an independent
    /// owned `TcpStream` handle to the same connection.
    ///
    /// Used by the asynchronous-socket completion path: a worker thread needs to
    /// perform a *blocking* read on the connection while the application keeps
    /// issuing `tcp_write`s (Future-form `AsynchronousSocketChannel.write`) on the
    /// same fd. Holding the per-fd `Mutex<TcpStream>` across a blocking read would
    /// deadlock those writes, so the reader takes a `try_clone()` handle (a second
    /// OS handle onto the same full-duplex socket) and reads on it lock-free while
    /// writes continue through the original entry. Returns an error if `fd` is not
    /// a live TCP stream.
    pub fn try_clone_tcp(&self, fd: FdId) -> Result<std::net::TcpStream, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp clone"))?;
        match &*entry {
            FileEntry::TcpStream(stream) => stream.lock().try_clone(),
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for tcp clone",
            )),
        }
    }

    /// Write to a TCP stream. Returns bytes written.
    pub fn tcp_write(&self, fd: FdId, data: &[u8]) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp write"))?;
        match &*entry {
            FileEntry::TcpStream(stream) => {
                let mut s = stream.lock();
                s.write(data)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for tcp write",
            )),
        }
    }

    /// Set non-blocking mode on a TCP stream or listener.
    pub fn tcp_set_nonblocking(&self, fd: FdId, nonblocking: bool) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(stream) => stream.lock().set_nonblocking(nonblocking),
            FileEntry::TcpListener { listener, .. } => listener.lock().set_nonblocking(nonblocking),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Get the local address of a TCP stream or listener.
    pub fn tcp_local_addr(&self, fd: FdId) -> Result<String, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(stream) => Ok(stream.lock().local_addr()?.to_string()),
            FileEntry::TcpListener { listener, .. } => {
                Ok(listener.lock().local_addr()?.to_string())
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Get the peer address of a TCP stream.
    pub fn tcp_peer_addr(&self, fd: FdId) -> Result<String, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(stream) => Ok(stream.lock().peer_addr()?.to_string()),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Check if a fd is ready for read/write (non-blocking poll).
    /// Returns (readable, writable).
    pub fn poll_ready(&self, fd: FdId) -> (bool, bool) {
        let Some(entry) = self.get_entry(fd) else {
            return (false, false);
        };
        match &*entry {
            FileEntry::UdpSocket(sock) => {
                // Non-destructive readiness probe via the OS `poll`
                // primitive: it neither flips the socket's persistent
                // blocking mode nor consumes the pending datagram, so a
                // concurrent blocking `udp_recv` on the same fd can never
                // observe a transient non-blocking window. The lock is
                // still taken so the raw handle stays valid for the call.
                let s = sock.lock();
                poll_socket_readiness(&*s)
            }
            FileEntry::TcpStream(stream) => {
                // Non-destructive readiness probe — see the UdpSocket arm.
                // Avoids the previous set_nonblocking(true)/peek/
                // set_nonblocking(false) dance, which could race a
                // concurrent blocking `tcp_read` into a spurious
                // `WouldBlock` and also clobbered a deliberately
                // non-blocking socket back to blocking.
                let s = stream.lock();
                poll_socket_readiness(&*s)
            }
            FileEntry::TcpListener { listener, pending } => {
                // A connection already stashed by a prior poll counts as
                // readable without touching the OS backlog.
                if !pending.lock().is_empty() {
                    return (true, false);
                }
                // Non-destructive readiness probe: `poll` reports the
                // listening socket readable when a connection is waiting
                // in the backlog, without `accept()`-ing it and without
                // touching the listener's blocking mode.
                let l = listener.lock();
                let (readable, _) = poll_socket_readiness(&*l);
                (readable, false) // listeners are readable (acceptable), not writable
            }
            FileEntry::FileRead(_) => (true, false),
            FileEntry::FileWrite(_) => (false, true),
            FileEntry::FileReadWrite(_) => (true, true),
            FileEntry::PipeRead(buf) => {
                let b = buf.lock();
                (!b.is_empty(), false)
            }
            FileEntry::PipeWrite(buf) => {
                let b = buf.lock();
                (false, b.len() < PIPE_BUFFER_CAPACITY)
            }
            _ => (false, false),
        }
    }

    // -----------------------------------------------------------------------
    // TCP socket options
    // -----------------------------------------------------------------------

    /// Set TCP_NODELAY on a TCP stream.
    pub fn tcp_set_nodelay(&self, fd: FdId, nodelay: bool) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => s.lock().set_nodelay(nodelay),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Get TCP_NODELAY on a TCP stream.
    pub fn tcp_nodelay(&self, fd: FdId) -> Result<bool, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => s.lock().nodelay(),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Set read timeout on a TCP stream.
    pub fn tcp_set_read_timeout(
        &self,
        fd: FdId,
        timeout: Option<Duration>,
    ) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => s.lock().set_read_timeout(timeout),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Get read timeout on a TCP stream.
    pub fn tcp_read_timeout(&self, fd: FdId) -> Result<Option<Duration>, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => s.lock().read_timeout(),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Set write timeout on a TCP stream.
    pub fn tcp_set_write_timeout(
        &self,
        fd: FdId,
        timeout: Option<Duration>,
    ) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => s.lock().set_write_timeout(timeout),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Set TTL on a TCP stream.
    pub fn tcp_set_ttl(&self, fd: FdId, ttl: u32) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => s.lock().set_ttl(ttl),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Set SO_KEEPALIVE on a TCP stream (via socket2).
    pub fn tcp_set_keepalive(&self, fd: FdId, keepalive: bool) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => {
                let stream = s.lock();
                let sock = socket2::SockRef::from(&*stream);
                sock.set_keepalive(keepalive)
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Get SO_KEEPALIVE on a TCP stream.
    pub fn tcp_keepalive(&self, fd: FdId) -> Result<bool, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => {
                let stream = s.lock();
                let sock = socket2::SockRef::from(&*stream);
                sock.keepalive()
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Set SO_LINGER on a TCP stream.
    pub fn tcp_set_linger(&self, fd: FdId, linger: Option<Duration>) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => {
                let stream = s.lock();
                let sock = socket2::SockRef::from(&*stream);
                sock.set_linger(linger)
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Get SO_LINGER on a TCP stream.
    pub fn tcp_linger(&self, fd: FdId) -> Result<Option<Duration>, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => {
                let stream = s.lock();
                let sock = socket2::SockRef::from(&*stream);
                sock.linger()
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Set SO_SNDBUF on a TCP stream.
    pub fn tcp_set_send_buffer_size(&self, fd: FdId, size: usize) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => {
                let stream = s.lock();
                let sock = socket2::SockRef::from(&*stream);
                sock.set_send_buffer_size(size)
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Get SO_SNDBUF on a TCP stream.
    pub fn tcp_send_buffer_size(&self, fd: FdId) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => {
                let stream = s.lock();
                let sock = socket2::SockRef::from(&*stream);
                sock.send_buffer_size()
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Set SO_RCVBUF on a TCP stream.
    pub fn tcp_set_recv_buffer_size(&self, fd: FdId, size: usize) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => {
                let stream = s.lock();
                let sock = socket2::SockRef::from(&*stream);
                sock.set_recv_buffer_size(size)
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Get SO_RCVBUF on a TCP stream.
    pub fn tcp_recv_buffer_size(&self, fd: FdId) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => {
                let stream = s.lock();
                let sock = socket2::SockRef::from(&*stream);
                sock.recv_buffer_size()
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Set SO_REUSEADDR on a TCP listener (via socket2).
    pub fn tcp_set_reuse_address(&self, fd: FdId, reuse: bool) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpListener { listener, .. } => {
                let listener = listener.lock();
                let sock = socket2::SockRef::from(&*listener);
                sock.set_reuse_address(reuse)
            }
            FileEntry::TcpStream(s) => {
                let stream = s.lock();
                let sock = socket2::SockRef::from(&*stream);
                sock.set_reuse_address(reuse)
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Estimate available bytes on a TCP stream using peek.
    ///
    /// Mirrors the rewrite documented at `fd_table.rs:1001` for
    /// `poll_ready`: the previous implementation flipped the socket
    /// to non-blocking, called `peek`, then flipped it back. That
    /// pattern (a) races against a concurrent blocking `tcp_read` on
    /// the same fd into a spurious `WouldBlock`, and (b) clobbers the
    /// blocking flag the caller had deliberately set — every
    /// `tcp_available` call would silently undo `set_nonblocking(true)`.
    ///
    /// New strategy: do a non-destructive readiness probe first. If
    /// the kernel reports the socket as not-readable, return 0 without
    /// touching the socket. If it is readable, the kernel already has
    /// buffered data and a `peek` is guaranteed not to block — no
    /// flag-toggling required.
    pub fn tcp_available(&self, fd: FdId) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => {
                let stream = s.lock();
                let (readable, _) = poll_socket_readiness(&*stream);
                if !readable {
                    // Nothing buffered in the kernel — no bytes
                    // available. Crucially, the socket's blocking flag
                    // is left exactly as the caller configured it.
                    return Ok(0);
                }
                // Readable per poll(); peek without flag toggling. On
                // a blocking stream that the kernel says has data this
                // returns immediately with the buffered byte count.
                let mut buf = [0u8; 8192];
                let avail = match stream.peek(&mut buf) {
                    Ok(n) => n,
                    // POLLIN with WouldBlock means readiness raced
                    // away (a concurrent reader drained the buffer).
                    // That's a snapshot estimate, return 0.
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => 0,
                    Err(_) => 0,
                };
                Ok(avail)
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    // -----------------------------------------------------------------------
    // UDP socket options
    // -----------------------------------------------------------------------

    /// Set SO_REUSEADDR on a UDP socket via socket2.
    pub fn udp_set_reuse_address(&self, fd: FdId, on: bool) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => {
                let sock = s.lock();
                let sock_ref = socket2::SockRef::from(&*sock);
                sock_ref.set_reuse_address(on)
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Set SO_BROADCAST on a UDP socket.
    pub fn udp_set_broadcast(&self, fd: FdId, on: bool) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => s.lock().set_broadcast(on),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Set TTL on a UDP socket.
    pub fn udp_set_ttl(&self, fd: FdId, ttl: u32) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => s.lock().set_ttl(ttl),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Set read timeout on a UDP socket.
    pub fn udp_set_read_timeout(
        &self,
        fd: FdId,
        timeout: Option<Duration>,
    ) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => s.lock().set_read_timeout(timeout),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Set SO_SNDBUF on a UDP socket.
    pub fn udp_set_send_buffer_size(&self, fd: FdId, size: usize) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => {
                let sock = s.lock();
                socket2::SockRef::from(&*sock).set_send_buffer_size(size)
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Set SO_RCVBUF on a UDP socket.
    pub fn udp_set_recv_buffer_size(&self, fd: FdId, size: usize) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => {
                let sock = s.lock();
                socket2::SockRef::from(&*sock).set_recv_buffer_size(size)
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Join a multicast group on a UDP socket (IPv4).
    pub fn udp_join_multicast_v4(
        &self,
        fd: FdId,
        multiaddr: &std::net::Ipv4Addr,
        interface: &std::net::Ipv4Addr,
    ) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => s.lock().join_multicast_v4(multiaddr, interface),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Leave a multicast group on a UDP socket (IPv4).
    pub fn udp_leave_multicast_v4(
        &self,
        fd: FdId,
        multiaddr: &std::net::Ipv4Addr,
        interface: &std::net::Ipv4Addr,
    ) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => s.lock().leave_multicast_v4(multiaddr, interface),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    // -----------------------------------------------------------------------
    // Pipe channels (in-memory)
    // -----------------------------------------------------------------------

    /// Create an in-memory pipe. Returns (read_fd, write_fd).
    pub fn open_pipe(&self) -> (FdId, FdId) {
        let buf = Arc::new(Mutex::new(VecDeque::with_capacity(
            PIPE_BUFFER_INITIAL_CAPACITY,
        )));
        let read_fd = self.alloc_fd_or_abort();
        let write_fd = self.alloc_fd_or_abort();
        let mut entries = self.entries.write();
        entries.insert(read_fd, Arc::new(FileEntry::PipeRead(buf.clone())));
        entries.insert(write_fd, Arc::new(FileEntry::PipeWrite(buf)));
        (read_fd, write_fd)
    }

    /// Read from the read end of a pipe.
    ///
    /// Returns the number of bytes read. An empty buffer is NOT unconditionally
    /// reported as EOF: `Ok(0)` (true EOF) is only returned once the write end
    /// has been closed. While a writer is still open and the buffer is empty,
    /// this returns `ErrorKind::WouldBlock` so a `FileInputStream` reader does
    /// not mistake "no data yet" for end-of-stream.
    ///
    /// The pipe buffer is shared via `Arc`: `PipeRead` holds one clone and each
    /// open `PipeWrite` holds another. So a `strong_count` of exactly 1 means
    /// only this read end remains — every writer has been dropped/closed.
    pub fn pipe_read(&self, fd: FdId, buf: &mut [u8]) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for pipe read"))?;
        match &*entry {
            FileEntry::PipeRead(pipe) => {
                let mut p = pipe.lock();
                let n = buf.len().min(p.len());
                if n == 0 {
                    // Buffer empty: distinguish a still-open writer from true EOF.
                    let writer_open = Arc::strong_count(pipe) > 1;
                    drop(p);
                    if writer_open {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "pipe empty, writer still open",
                        ));
                    }
                    return Ok(0);
                }
                for (i, b) in p.drain(..n).enumerate() {
                    buf[i] = b;
                }
                Ok(n)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for pipe read",
            )),
        }
    }

    /// Write to the write end of a pipe. Returns bytes written.
    pub fn pipe_write(&self, fd: FdId, data: &[u8]) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for pipe write"))?;
        match &*entry {
            FileEntry::PipeWrite(pipe) => {
                if data.is_empty() {
                    return Ok(0);
                }
                let mut p = pipe.lock();
                let available = PIPE_BUFFER_CAPACITY.saturating_sub(p.len());
                if available == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "pipe buffer full",
                    ));
                }
                let n = data.len().min(available);
                p.extend(&data[..n]);
                Ok(n)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for pipe write",
            )),
        }
    }

    // =========================================================================
    // TLS operations
    // =========================================================================

    /// Connect via TLS to the given host:port. Returns the fd_id.
    ///
    /// # Security
    ///
    /// `host`/`port` are used **verbatim** to connect with no sandbox/SSRF
    /// check performed here. When these originate from Java-controlled input,
    /// the caller (the VM / native-io sandbox layer) MUST sanitize them
    /// before calling — this function deliberately does not enforce that, to
    /// avoid duplicating (and potentially conflicting with) the VM's own
    /// sandbox policy.
    pub fn open_tls_connect(&self, host: &str, port: u16) -> Result<FdId, io::Error> {
        // fds only need to be unique, not contiguous — on overflow we
        // simply fail without rolling the counter back (a `fetch_sub`
        // rollback would be racy and pointless).
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            return Err(io::Error::other("file descriptor limit exceeded"));
        }
        let addr = format!("{}:{}", host, port);
        let tcp = std::net::TcpStream::connect(&addr)?;
        let connector = native_tls::TlsConnector::new()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let tls_stream = connector
            .connect(host, tcp)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::TlsStream(Mutex::new(tls_stream))));
        Ok(fd)
    }

    /// Read from a TLS stream.
    pub fn tls_read(&self, fd: FdId, buf: &mut [u8]) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tls read"))?;
        match &*entry {
            FileEntry::TlsStream(stream) => {
                let mut s = stream.lock();
                s.read(buf)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for tls read",
            )),
        }
    }

    /// Write to a TLS stream.
    pub fn tls_write(&self, fd: FdId, data: &[u8]) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tls write"))?;
        match &*entry {
            FileEntry::TlsStream(stream) => {
                let mut s = stream.lock();
                s.write(data)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for tls write",
            )),
        }
    }
}

impl Default for FileDescriptorTable {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for FileDescriptorTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileDescriptorTable")
            .field("open_fds", &self.entries.read().len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering as AtomOrd};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

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
                FileEntry::UdpSocket(sock) => {
                    let sock = sock.lock();
                    poll_socket_readiness(&*sock)
                }
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
}
