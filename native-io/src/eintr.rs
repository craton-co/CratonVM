// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! EINTR-transparent socket I/O.
//!
//! A blocking `read`/`recv`/`write`/`send` that a signal interrupts before it
//! transferred a single byte fails with `EINTR` (errno 4,
//! [`std::io::ErrorKind::Interrupted`]). POSIX requires the caller to reissue
//! the call; it is never a transport error. Surfacing it to Java turns an
//! ordinary signal into a random mid-request `IOException`, because nothing in
//! `java.net` or `javax.net.ssl` has an "interrupted, try again" state — the
//! JDK's own native layer retries below the Java surface.
//!
//! **CratonVM generates these signals itself.** `jit::xt_root_scan` sends
//! `SIGUSR2` to every thread to take it over for a cross-thread stop-the-world
//! root scan, and `process.rs` children deliver `SIGCHLD`. The handler is
//! installed with `SA_RESTART`, which is not enough on two counts:
//!
//! * `poll(2)` is *never* restarted by `SA_RESTART` (see `net::net_poll_raw`);
//! * a socket with `SO_RCVTIMEO`/`SO_SNDTIMEO` set is never restarted either —
//!   Linux `signal(7)` lists exactly this exception, and every CratonVM HTTP
//!   and TLS client path sets a receive timeout before each read to keep the
//!   caller's deadline honest.
//!
//! So the TLS client handshake, which is a bare `read_tls`/`write_tls` pair on
//! a socket carrying `SO_RCVTIMEO`, sees `EINTR` whenever a JIT root scan lands
//! while it is parked. That is the defect recorded in
//! `jdk-httpclient-sslbundle-tls-handshake-eintr-FIXED-20260806.md` (a path
//! relative to the internal tree's own root — see `doc_citation_paths`).
//!
//! Note the asymmetry with `std`: `write_all`, `read_exact` and `read_to_end`
//! already reissue on `Interrupted` *and* advance past the bytes they placed.
//! Do not wrap those in [`retry_eintr`] — it can only restart the whole call,
//! which for a partially-completed write means resending from offset 0.
//!
//! Wrap the socket in [`EintrIo`] (borrowed) or [`EintrStream`] (owned) and no
//! layer above it — rustls, `native_tls`, or a hand-written read loop — can
//! observe the condition.
//!
//! # Where NOT to use it
//!
//! Retrying *in place* reissues the syscall with the receive timeout it already
//! had, and Linux restarts that timer after every interrupted `recv`. Anywhere
//! the caller re-arms `SO_RCVTIMEO` from a wall-clock deadline before each
//! read — `net_phase_e::HttpDeadlineReader`, `net::net_poll_raw` — the EINTR
//! belongs one level up, so the next pass recomputes the remaining time. Use
//! [`is_eintr`] there and loop in the caller. This module is for the sites that
//! have no such loop, which is every TLS handshake in the tree.
//!
//! # Reproducing the defect
//!
//! Two debug-only knobs, both off unless set, make the failure deterministic on
//! any platform (see [`cratonvm_types::IoFlags`]):
//!
//! * `CRATONVM_DBG_EINTR_INJECT=<n>` — synthesise an `EINTR` on every *n*-th
//!   operation that passes through this module.
//! * `CRATONVM_DBG_EINTR_NO_RETRY=1` — do not retry.
//!
//! One binary, two arms: `INJECT` alone must stay green, `INJECT` plus
//! `NO_RETRY` must reproduce the reported `IOException`.
//!
//! `NO_RETRY` governs [`retry_eintr`], [`EintrIo`] and [`EintrStream`] — i.e.
//! every retry this module introduced, which is the whole TLS handshake family.
//! It does **not** reach the hand-written EINTR arms that predate it
//! (`net::read0`/`write0`/`net_poll_raw`, `socket_channel::try_read_nb`,
//! `http_url_connection::read_eof_tolerant`) or `socket_channel`'s TCP accept
//! arm, all of which retry unconditionally. So on Linux the `NO_RETRY` arm is
//! the pre-branch behaviour *of the handshake*, not of the whole crate.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};

/// True for the error a POSIX I/O syscall reports when a signal interrupted it
/// before it transferred anything.
///
/// The raw-errno arm is not redundant: an error that has been round-tripped
/// through a wrapper which rebuilt it from `errno` — or one produced by a
/// non-`std` FFI call — can carry errno 4 without `ErrorKind::Interrupted`.
#[inline]
#[must_use]
pub fn is_eintr(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Interrupted || error.raw_os_error() == Some(4)
}

/// Run `op`, reissuing it for as long as it reports EINTR.
///
/// Use this for whole operations that are not a plain `Read`/`Write` call —
/// `rustls`' `Stream::flush`, for instance, drives `complete_io` internally and
/// so can surface the socket's `EINTR` from a method [`EintrIo`] never sees.
pub fn retry_eintr<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match op() {
            Err(error) if is_eintr(&error) && !no_retry() => continue,
            result => return result,
        }
    }
}

/// A borrowed socket whose `Read`/`Write` never surface EINTR.
///
/// This is the shape the `read_tls`/`write_tls` call sites want:
/// `conn.read_tls(&mut EintrIo::new(&mut stream.sock))` leaves every
/// surrounding type signature alone.
pub struct EintrIo<'a, S: ?Sized>(&'a mut S);

impl<'a, S: ?Sized> EintrIo<'a, S> {
    /// Borrow `inner` for the duration of one EINTR-transparent operation.
    pub fn new(inner: &'a mut S) -> Self {
        Self(inner)
    }
}

impl<S: Read + ?Sized> Read for EintrIo<'_, S> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            match injected_eintr().map_or_else(|| self.0.read(buffer), Err) {
                Err(error) if is_eintr(&error) && !no_retry() => continue,
                result => return result,
            }
        }
    }
}

impl<S: Write + ?Sized> Write for EintrIo<'_, S> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        loop {
            match injected_eintr().map_or_else(|| self.0.write(data), Err) {
                Err(error) if is_eintr(&error) && !no_retry() => continue,
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        retry_eintr(|| self.0.flush())
    }
}

/// An owned socket whose `Read`/`Write` never surface EINTR.
///
/// Needed where a TLS library takes the socket by value — `native_tls`'s
/// `TlsConnector::connect`, `rustls`' `StreamOwned::new`. [`EintrStream::get_ref`]
/// reaches the socket again for `set_read_timeout` and friends.
///
/// `Debug` is derived because `native_tls::HandshakeError` requires it of the
/// stream type it carries.
#[derive(Debug)]
pub struct EintrStream<S>(S);

impl<S> EintrStream<S> {
    /// Take ownership of `inner`.
    pub fn new(inner: S) -> Self {
        Self(inner)
    }

    /// The wrapped socket.
    pub fn get_ref(&self) -> &S {
        &self.0
    }

    /// The wrapped socket, mutably.
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.0
    }

    /// Give the socket back.
    pub fn into_inner(self) -> S {
        self.0
    }
}

impl<S: Read> Read for EintrStream<S> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        EintrIo::new(&mut self.0).read(buffer)
    }
}

impl<S: Write> Write for EintrStream<S> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        EintrIo::new(&mut self.0).write(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        retry_eintr(|| self.0.flush())
    }
}

/// `CRATONVM_DBG_EINTR_NO_RETRY` — reproduce the pre-fix behaviour.
#[inline]
fn no_retry() -> bool {
    crate::io_flags().dbg_eintr_no_retry
}

/// Every `n`-th operation, hand back a synthetic EINTR instead of doing it.
///
/// The counter is process-wide and deliberately so: the point is to land the
/// injection on whichever socket operation happens to be in flight, the way a
/// real signal does.
fn injected_eintr() -> Option<io::Error> {
    // Floor of 2. A period of 1 would inject on the retry as well, so a
    // correctly-retrying caller would spin forever and the switch would look
    // like a hang instead of like the defect.
    let period = match crate::io_flags().dbg_eintr_inject {
        Some(period) => period.max(2),
        None => return None,
    };
    static OPS: AtomicU64 = AtomicU64::new(0);
    if OPS.fetch_add(1, Ordering::Relaxed) % period != period - 1 {
        return None;
    }
    // On unix, errno 4 renders as "Interrupted system call (os error 4)" — the
    // exact text of the reported failure. Windows has no EINTR and would render
    // error 4 as something unrelated, so spell the message out there instead;
    // the point of the switch is that the two hosts produce the same evidence.
    #[cfg(unix)]
    {
        Some(io::Error::from_raw_os_error(4))
    }
    #[cfg(not(unix))]
    {
        Some(io::Error::new(
            io::ErrorKind::Interrupted,
            "Interrupted system call (os error 4)",
        ))
    }
}

#[cfg(test)]
mod tests;
