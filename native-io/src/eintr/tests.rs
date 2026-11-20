use super::*;

/// A socket that reports EINTR `pending` times and then behaves.
struct Flaky {
    pending: usize,
    payload: Vec<u8>,
    written: Vec<u8>,
    flushes_pending: usize,
}

impl Flaky {
    fn new(pending: usize) -> Self {
        Self {
            pending,
            payload: b"handshake".to_vec(),
            written: Vec::new(),
            flushes_pending: 0,
        }
    }

    fn eintr(&mut self) -> Option<io::Error> {
        if self.pending == 0 {
            return None;
        }
        self.pending -= 1;
        // Alternate the two shapes a real interrupted syscall can take: a
        // classified `Interrupted`, and a bare errno 4 from an FFI wrapper
        // that rebuilt the error itself.
        Some(if self.pending % 2 == 0 {
            io::Error::new(io::ErrorKind::Interrupted, "Interrupted system call")
        } else {
            io::Error::from_raw_os_error(4)
        })
    }
}

impl Read for Flaky {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if let Some(error) = self.eintr() {
            return Err(error);
        }
        let n = self.payload.len().min(buffer.len());
        buffer[..n].copy_from_slice(&self.payload[..n]);
        Ok(n)
    }
}

impl Write for Flaky {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if let Some(error) = self.eintr() {
            return Err(error);
        }
        self.written.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.flushes_pending > 0 {
            self.flushes_pending -= 1;
            return Err(io::Error::from_raw_os_error(4));
        }
        Ok(())
    }
}

#[test]
fn raw_errno_four_counts_as_eintr() {
    assert!(is_eintr(&io::Error::from_raw_os_error(4)));
    assert!(is_eintr(&io::Error::new(
        io::ErrorKind::Interrupted,
        "Interrupted system call"
    )));
    assert!(!is_eintr(&io::Error::from(io::ErrorKind::TimedOut)));
    assert!(!is_eintr(&io::Error::from(io::ErrorKind::WouldBlock)));
}

#[test]
fn borrowed_reader_absorbs_every_eintr() {
    let mut socket = Flaky::new(7);
    let mut buffer = [0u8; 16];
    let n = EintrIo::new(&mut socket)
        .read(&mut buffer)
        .expect("EINTR must not escape EintrIo");
    assert_eq!(&buffer[..n], b"handshake");
    assert_eq!(socket.pending, 0, "every injected EINTR should be consumed");
}

#[test]
fn borrowed_writer_absorbs_every_eintr() {
    let mut socket = Flaky::new(5);
    let n = EintrIo::new(&mut socket)
        .write(b"client hello")
        .expect("EINTR must not escape EintrIo");
    assert_eq!(n, "client hello".len());
    assert_eq!(socket.written, b"client hello");
}

#[test]
fn owned_stream_absorbs_every_eintr() {
    let mut stream = EintrStream::new(Flaky::new(4));
    let mut buffer = [0u8; 16];
    let n = stream.read(&mut buffer).expect("EINTR must not escape");
    assert_eq!(&buffer[..n], b"handshake");
    assert_eq!(stream.get_ref().pending, 0);
}

#[test]
fn flush_is_retried_too() {
    // `rustls`' `Stream::flush` drives `complete_io`, so the socket's EINTR
    // arrives from a method the `Read`/`Write` wrappers never see.
    let mut socket = Flaky::new(0);
    socket.flushes_pending = 3;
    EintrIo::new(&mut socket)
        .flush()
        .expect("EINTR must not escape");
    assert_eq!(socket.flushes_pending, 0);
}

#[test]
fn retry_eintr_passes_other_errors_through() {
    let mut calls = 0;
    let error = retry_eintr(|| -> io::Result<()> {
        calls += 1;
        Err(io::Error::from(io::ErrorKind::ConnectionReset))
    })
    .expect_err("a real transport error must not be retried");
    assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
    assert_eq!(calls, 1);
}
