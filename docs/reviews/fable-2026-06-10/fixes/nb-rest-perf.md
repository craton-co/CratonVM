# Fix: nb-rest-perf — `socket_accept` holds process-wide registry lock across blocking accept (P1)

## Finding
`native-builtins/src/plain_socket.rs` `socket_accept` (the legacy
`PlainSocketImpl`/`NioSocketImpl`/`PlainServerSocketImpl` blocking accept path).

In the no-`SO_TIMEOUT` branch, the function took `registry().read()` and then
called the **blocking** `s.socket.accept()` *while still holding the read
guard*. Every other socket operation in this module goes through `with_socket`,
which takes `registry().write()`. A read guard held across an indefinitely
blocking `accept()` therefore blocks all writers, so a single blocking accept on
a daemon server serialized ALL socket I/O process-wide until a connection
arrived (a DoS-grade contention bug — review item P1, HIGH).

The `SO_TIMEOUT` branch already dropped the guard around the sleep, but still
re-acquired `registry().read()` on *every* 5 ms poll iteration to call
`accept()`, so it too contended with writers each tick.

## Root cause
The blocking syscall was performed on the socket *in place* inside the registry
map, forcing the lock to be held for the syscall's entire (unbounded) duration.

## Exact change
Rewrote `socket_accept` to clone the listening socket handle OUT of the registry
under a short read lock, release the lock, then block on the clone — exactly the
pattern the sibling socket module already uses
(`net_phase_e::re10_start_server`, `let listener = ... l.try_clone()?`).

- New short critical section: `let (listener, timeout) = { let g =
  registry().read(); ... s.socket.try_clone()?; (clone, s.so_timeout_ms) };`.
  `socket2::Socket::try_clone()` dups the underlying fd/handle, so accepting on
  the clone is equivalent to accepting on the original on both POSIX and Windows.
- No-timeout branch: `listener.accept()` on the local clone — **no registry lock
  held** across the blocking call.
- `SO_TIMEOUT` branch: poll `listener.accept()` (the local clone) in the busy
  loop; no per-iteration registry lock at all. Behavior/timing preserved
  (same deadline, same 5 ms sleep, same `WouldBlock`/error/timeout handling and
  return values).

Behavior preservation notes:
- Return values, error messages (`socketAccept: ...`, `socket gone`,
  `accept timed out`), and the post-accept new-`SocketImpl` registration are all
  unchanged.
- The old timeout path toggled `set_nonblocking` on the *registered* socket
  (visible to other threads mid-accept); the new path toggles it on the local
  clone only, which is strictly more correct (no shared-state side effect) and
  leaves the registered socket's blocking mode untouched.
- `try_clone()` failure is surfaced as the same `IOException` shape via `ioex`.

## P3 (LOW) note — SO_TIMEOUT busy-poll
The `SO_TIMEOUT` path still busy-polls with `set_nonblocking(true)` +
`thread::sleep(5ms)` per iteration. Left as-is for correctness (a real
edge-triggered wait — `poll`/`WSAPoll` with a deadline — would be the proper fix
but is non-trivial and out of scope for this perf change). The contention
concern is fully resolved because the poll loop no longer holds the registry
lock.

## Files touched
- `native-builtins/src/plain_socket.rs` (only `socket_accept`; added one
  regression test).

## Tests added
- `blocking_accept_does_not_hold_registry_lock` (in the existing `#[cfg(test)]
  module): registers a bound/listening server, spawns a thread that clones the
  listener out and blocks on `accept()`, then asserts a concurrent
  `with_socket` write on an *unrelated* socket completes while the accept is
  blocked (would deadlock/stall under the old code). Unblocks via a loopback
  client connect and joins. Uses only APIs already present in the test module.

## Follow-up & risk
- Risk: LOW. `try_clone` is the established pattern in `net_phase_e.rs` on the
  same platform; accept-on-dup is well-defined. No public signature or
  registration changed.
- Follow-up: P3 edge-triggered SO_TIMEOUT accept; and the review's broader
  suggestion (#5) to give the plain-socket path its own self-contained
  read/write surface instead of depending on `net_phase_e`.
