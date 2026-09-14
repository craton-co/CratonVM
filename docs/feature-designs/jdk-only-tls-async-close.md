# P3-D — async close on TLS streams: the design

**Status: DESIGN ONLY. No Rust was written for this document and none of the
four sites was edited.** Nothing here was built or run — this lane has no
build. Every claim below is either a quotation from source in this tree, a
quotation from the locked dependency sources in this host's cargo registry, or
a language-reference fact about the edition this workspace compiles under. Where
something is a contract rather than a measurement it says so.

Roadmap entry: `docs/feature-designs/jdk-only-completion-roadmap.md` §P3-D,
which schedules this lane as `DESIGN FIRST: P3-D (the obvious fix is unsound —
do not bundle it)`.

Records this closes reading on, and does not close:
`docs/known-issues/jdk-only/W7-53-blocking-close-family.md` ("The four TLS
sites") and `W7-61-sslengine-layout-and-tls-blocking.md`.

---

## 0. The headline result: the concern is REAL, and it is real for a stronger
## reason than the record states

W7-53's third pass asserted that a socket-readiness gate deadlocks the ordinary
HTTP-over-TLS shape, and quoted `rustls` **0.23.42** for it. **This tree does not
build 0.23.42.** `Cargo.lock` pins

```
name = "rustls"
version = "0.23.38"
```

so the record's evidence was read off a version that is not the one shipped. I
re-derived it against `rustls-0.23.38`, which is present in this host's registry,
and the concern survives **byte-identically** — plus one thing the record does
not say, which strengthens it from "plausible" to "exact in both directions".

`rustls-0.23.38/src/stream.rs`, `Stream::prepare_read` — the only path
`StreamOwned::read` takes (`StreamOwned::read` is `self.as_stream().read(buf)`,
and `Stream::read` is `self.prepare_read()?; self.conn.reader().read(buf)`):

```rust
    fn prepare_read(&mut self) -> Result<()> {
        self.complete_prior_io()?;

        // We call complete_io() in a loop since a single call may read only
        // a partial packet from the underlying transport. A full packet is
        // needed to get more plaintext, which we must do if EOF has not been
        // hit.
        while self.conn.wants_read() {
            if self.conn.complete_io(self.sock)?.0 == 0 {
                break;
            }
        }

        Ok(())
    }
```

`rustls-0.23.38/src/common_state.rs:674`:

```rust
    pub fn wants_read(&self) -> bool {
        // We want to read more data all the time, except when we have unprocessed plaintext.
        // ...
        self.received_plaintext.is_empty()
            && !self.has_received_close_notify
            && (self.may_send_application_data || self.sendable_tls.is_empty())
    }
```

So: **buffered plaintext ⇒ `wants_read()` is `false` ⇒ `prepare_read` does no
socket I/O at all and `reader().read(buf)` returns from memory.** A gate that
polls the socket before that read parks a reader whose answer was already in
hand, waiting for an edge the peer will never produce — because the peer is
waiting for the reply this reader is now never going to send. That is a
deadlock on request/response over TLS, not on an edge case.

### The part the record does not establish, and which decides the design

The record treats `wants_read()` as "exact" by inspection. It is exact, and the
proof is one function further down. The worry a reviewer should have is a
**false `true`**: rustls could hold a fully-received but not-yet-decrypted record
in its own deframer buffer while `received_plaintext` is empty, in which case
`wants_read()` would say `true`, we would poll a socket with nothing left to
deliver, and we would hang for the same reason. It cannot:
`ConnectionCommon::complete_io` (`rustls-0.23.38/src/conn.rs:602`) runs
`self.process_new_packets()` after **every** pass of its read loop —

```rust
            if let Err(e) = self.process_new_packets() {
```

— so at the instant any `complete_io` returns, everything deframable has already
been deframed into `received_plaintext`. `wants_read() == true` therefore means
*genuinely nothing decryptable is buffered*, and `wants_read() == false` means
*a read will not touch the socket*. The predicate is exact in **both**
directions, which is the whole reason the pilot is expressible at all.

### And the part that makes the other three sites NOT a copy-paste

`native_tls::TlsStream::buffered_read_size` is documented in
`native-tls-0.2.18/src/lib.rs:679` as

> "Returns the number of bytes that can be read without resulting in any
> network calls."

which reads like the same guarantee. On the Windows backend it is
(`native-tls-0.2.18/src/imp/schannel.rs:394`)

```rust
    pub fn buffered_read_size(&self) -> Result<usize, Error> {
        Ok(self.0.get_buf().len())
    }
```

and `schannel`'s `get_buf` returns the **decrypted** buffer only
(`&self.dec_in.get_ref()[self.dec_in.position() as usize..]`). Bytes sitting in
schannel's *encrypted* input buffer — a whole record already off the wire but
not yet decrypted — are not counted. If that state is reachable between calls,
`buffered_read_size() == 0` is a **false zero**, and a false zero is precisely
the deadlock direction. The openssl arm (`SSL_pending`) has the same shape and
the same question.

**This is why the pilot is client-rustls-only and why the other three sites must
not copy it on the strength of "the same screen exists there".** For rustls the
screen is proved exact from the crate's own source. For native-tls it is a
documented promise whose Windows implementation has a visible gap that nothing
in this tree has measured. The roadmap's "the screen must be asked under the
stream mutex (`conn.wants_read()` / `TlsStream::buffered_read_size()`)" is right
about the *shape* and understates the difference in *standing* between the two.

---

## 1. The four sites

| # | site | file:line | direction | assembler |
|---|---|---|---|---|
| 1 | `t27_tls::rustls_stream_read` — **client arm** | `native-builtins/src/t27_tls.rs:4012` (fn), arm at `:4029`–`:4048` | read | `StreamOwned<ClientConnection, TcpStream>` |
| 1b | `t27_tls::rustls_stream_read` — server arm | `native-builtins/src/t27_tls.rs:4049`–`:4078` | read | `TlsServerStream` = `StreamOwned<ServerConnection,_>` \| `native_tls::TlsStream` \| `#[cfg(unix)] openssl::ssl::SslStream` |
| 2 | `t27_tls::rustls_stream_write` | `native-builtins/src/t27_tls.rs:4086` | write | both of the above |
| 3 | `servlet::s2_tls_read_direct` | `native-builtins/src/servlet.rs:2577` | read | `TlsClientStream` = `native_tls::TlsStream` \| `#[cfg(unix)] SslStream` |
| 4 | `servlet::s2_tls_write` | `native-builtins/src/servlet.rs:2660` | write | as above |

Sites 3 and 4 forward to sites 1 and 2 for `id >= RUSTLS_SOCK_ID_BASE`
(`servlet.rs:2578`, `:2661`) **before** taking their own registry lock, which
matters for §2.

Registry entries and the second handle:

| table | entry | second handle |
|---|---|---|
| `t27_tls::sreg().client_streams` | `TlsClientStreamEntry` (`t27_tls.rs:1469`) | `raw: Option<TcpStream>` (`:1481`) |
| `t27_tls::sreg().server_streams` | `TlsServerStreamEntry` (`:1489`) | `raw: Option<TcpStream>` (`:1493`) |
| `servlet::s2_registry().tls_streams` | `TlsEntry` (`servlet.rs:2084`) | `raw: Option<TcpStream>` (`:2094`) |

Client-side `raw` is populated at exactly **two** construction sites,
`t27_tls.rs:3263` and `t27_tls.rs:3683`, both spelled
`let raw = stream.sock.try_clone().ok();`.

---

## 2. Lock inventory and the ordering rule

Every lock reachable on the four paths, in the order a request meets them.

| id | lock | type | declared | scope |
|---|---|---|---|---|
| **L3** | `servlet::s2_registry()` | `parking_lot::Mutex<SocketRegistry>` | `servlet.rs:2139` | process-wide: every synthetic socket, listener, datagram and native-tls stream |
| **L4** | `servlet::tls_readahead()` | `parking_lot::Mutex<HashMap<i32, TlsReadahead>>` | `servlet.rs:2492` | process-wide, 32 KiB plaintext readahead per id |
| **L1** | `t27_tls::sreg()` | `parking_lot::Mutex<ServerRegistry>` | `t27_tls.rs:1534` | process-wide: every rustls listener and stream |
| **L2** | per-stream assembler mutex | `Arc<parking_lot::Mutex<…>>` | `t27_tls.rs:1473`, `:1491`; `servlet.rs:2088` | one connection |
| **L5** | `FdTable::entries` / `nonblocking` | `RwLock` | `native-api/src/fd_table.rs` | **not on this path** — TLS sockets are `TcpStream`s in these registries, not fd-table entries |
| **L6** | the VM safepoint protocol | — | — | implicit: a thread parked on a plain mutex inside a native never reaches a safepoint |

All four are **`parking_lot::Mutex`**, which is neither reentrant nor poisoned:
a recursive `lock()` on the same mutex from the same thread is a hard deadlock,
not a panic. `try_lock` returns `Option`, which is how `rustls_stream_close`
spells it at `t27_tls.rs:4185`.

### The rule

> **R1 — L1 and L3 are lookup locks. Never hold either across a syscall, a
> poll, or an acquisition of L2.**
>
> **R2 — never hold two of {L1, L3, L4} at once, in any order.**
>
> **R3 — L2 is acquired only with no registry lock held, and released before
> any registry lock is retaken.** The permitted shape is a *sequence*, never a
> nesting: `L1 → drop → L2 → drop → L1 → drop → L2 → drop → …`.
>
> **R4 — never hold L2 across the poll.** The parked reader holds L2; a closer
> that needed L2 would serialise behind it, which is the current defect, and a
> second reader that took L2 to reach `get_ref()` would deadlock against the
> first.

R1–R4 hold in the tree today and the fix must preserve them, not invent them:

* `rustls_stream_read:4022` clones the `Arc` under L1 in a block and drops L1
  before `stream.lock()` at `:4030`. Its doc comment states the reason,
  including the safepoint half — "a thread parked on a plain mutex inside a
  native call never reaches a safepoint, so a concurrent STW waits for it
  forever".
* `rustls_stream_close:4136` removes under L1 in a block, then does the
  `shutdown` and the `try_lock` outside it.
* `s2_tls_read_direct:2589` does the same with L3.
* `s2_tls_fill_readahead:2540` calls the blocking read **before** taking L4
  (`:2547`), so L4 is never held across I/O.
* `s2_tls_close:2693` takes L4 (`s2_tls_discard_readahead`) and releases it
  before either the rustls delegation (`:2695`) or the L3 removal (`:2699`).

The registry-guard lock cycle this codebase has paid for before
(`registry-guard-across-a-blocking-call-is-a-lock-cycle`) is exactly an R1
violation, and the `DatagramChannel.read` wedge W7-53 fixed is exactly an R2/R1
violation. The proposed loop adds **no new lock** and no new ordering — it adds
one more `L2 → drop → L1 → drop` cycle per pass.

### The one genuinely new hazard the loop introduces

The loop must evaluate a predicate **under L2** and then **branch on it after
releasing L2**. Written naively that is an `if let` / `match` whose scrutinee
holds the guard, and the arm that blocks then runs with the guard alive.

**This workspace is edition 2021.** `Cargo.toml`'s `[workspace.package]` sets
`edition = "2021"` / `rust-version = "1.80"`, and both `native-builtins` and
`native-io` inherit it (`edition.workspace = true`). Under editions ≤ 2021 the
temporaries of an `if let` scrutinee live to the end of the **whole** `if let`
expression, **including the `else` block**; edition 2024's if-let rescoping is
what changes that. So

```rust
// WRONG under edition 2021: the guard is alive inside the `else`,
// and the `else` is where we park.
if let Some(n) = stream.lock().buffered() {
    …
} else {
    poll_and_wait(…);            // holding L2 — deadlocks every closer
}
```

is a live hazard in this crate today, and it is the
`iflet-mutex-guard-held-across-blocking-else-branch` shape this codebase already
has a record for.

**Do not plan around an edition bump.** `match` scrutinee temporaries live for
the whole `match` in *every* edition, so the same mistake spelled `match
stream.lock().screen() { … }` is a hazard even after 2024. The rule is
therefore structural, not edition-dependent:

> **R5 — the screen is computed into a plain local, the guard is bound by an
> explicit `let` and dropped by an explicit `drop(guard);`, and only then may
> the code branch.** No `stream.lock()` in the scrutinee of an `if let`, `match`
> or `while let` whose arms can block.

The existing code already obeys R5 by accident: `rustls_stream_read:4030` binds
`let mut e = stream.lock();` and calls `drop(e);` at `:4043` explicitly rather
than relying on scope end.

---

## 3. The pilot, in full

**Scope: `t27_tls::rustls_stream_read`'s client arm only** — `t27_tls.rs:4029`
–`:4048`. Not the server arm, not either write, not `servlet.rs`. The reasons
are §0 (only this arm's screen is proved exact) plus: `StreamOwned<ClientConnection,
TcpStream>` has no `#[cfg]` arms anywhere in its type, so the whole change is
portable safe Rust that this Windows host can type-check, and it needs no enum
match.

### 3.1 One struct change

`TlsClientStreamEntry::raw: Option<TcpStream>` → `Option<Arc<TcpStream>>`
(`t27_tls.rs:1481`), with the two construction sites (`:3263`, `:3683`) becoming
`let raw = stream.sock.try_clone().ok().map(Arc::new);`.

`rustls_stream_close`'s consumer at `:4171` is
`client.as_ref().and_then(|e| e.raw.as_ref())` feeding
`raw.shutdown(Shutdown::Both)`; `shutdown` takes `&self` and `Arc<TcpStream>`
derefs to `TcpStream`, so that call site compiles unchanged. The server arm's
`:4172` sibling is untouched by the pilot and keeps `Option<TcpStream>`, so the
two branches of that `for` array literal must be reconciled — `[client…, server…]`
currently builds a `[Option<&TcpStream>; 2]`. Under the pilot the two halves have
different types and the loop has to become two statements. **This is the one
place the pilot forces a mechanical edit outside the read function**, and it is
worth saying out loud because it is exactly the sort of thing that turns a
"one arm" change into a whole-file change if it is discovered late.

#### Why `Arc` and not `try_clone()` per read

W7-53's second lane declined the fix partly because `try_clone()`ing `raw` on
every blocking read duplicates a descriptor at request rate on the
Tomcat/WildFly path, and it could not price that. It does not have to:
**nineteen already-fixed sites in this family park holding an `Arc<TcpStream>`
cloned out of a registry**, and it is the `Arc` — not a duplicated descriptor —
that keeps the handle alive under a parked thread. An `Arc::clone` is a
refcount bump; a `try_clone` is two syscalls and a handle. Take the `Arc`.

This also **retires trap 3**, and trap 3 is real rather than theoretical:
`rustls_stream_close:4139` does `reg.client_streams.remove(&id)`, so the entry
— and with it the owned `TcpStream` in `raw` — is dropped when that function
returns. A poller that snapshotted `raw`'s integer fd before the close would be
polling a number the OS is free to have recycled onto an unrelated file. Under
`Arc<TcpStream>` the descriptor outlives every holder and the question does not
arise. **Do not snapshot the integer.**

### 3.2 The loop

Pseudocode, and every step exists because something below would otherwise break:

```
resolve id under L1, clone `Arc<Mutex<StreamOwned<…>>>` AND `Arc<TcpStream>`; drop L1
loop {
    // (a) THE SCREEN — under L2, per R5
    let guard = stream.lock();
    let needs_socket = guard.conn.wants_read();
    drop(guard);
    if !needs_socket {
        break;                      // buffered plaintext, or close_notify: read now
    }

    // (b) the close question, asked with NO lock held
    if !still_registered(id) {      // takes and releases L1
        return Err(Interrupted, "socket closed");
    }

    // (c) the bounded park
    match poll_stream_readable(&raw, TLS_CLOSE_POLL_MS) {
        None            => break,   // no poll primitive in this build: one plain read
        Some(Err(_))    => break,   // poll itself failed: one plain read, do not spin
        Some(Ok(true))  => break,   // readable (or POLLERR/POLLHUP) — the read surfaces it
        Some(Ok(false)) => {        // slice expired: NOT an outcome, re-ask and loop
            if deadline_expired() { return Err(TimedOut, …); }
            continue;
        }
    }
}
// (d) exactly one TLS op, under L2, unchanged from today
let mut e = stream.lock();
let result = crate::http_url_connection::read_eof_tolerant(&mut *e, buf);
drop(e);
rustls_classify_after_block(id, result)
```

* **(a) before (c)** is §0. Reversing them is the unsound design.
* **(b) after the poll on the next pass, and before the first poll on this
  one.** W7-53's landed mechanism asks the registry *after* the poll so a close
  that lands while parked is seen on the very next pass and a close that raced
  a readiness edge still wins. Placing (b) at the top of the loop body gives the
  same ordering, because the `continue` from (c) returns to it.
* **(d) is untouched.** The record's residual — a reader parked *inside*
  `complete_io` between the TCP segments of one 16 KiB record — survives, and is
  priced in §6.

### 3.3 The primitive: no eighth poll binding

`cratonvm_native_io::net::poll_stream_readable(&TcpStream, i32) ->
Option<std::io::Result<bool>>` is `pub` (`native-io/src/net.rs:2434`) precisely
so `native-builtins` can call it across the crate boundary — its doc comment says
so, naming W2-2's out-of-file patch. `native-builtins` already depends on
`cratonvm_native_io` and `t27_tls.rs` already imports from it
(`use cratonvm_native_io::eintr::EintrIo;`, `t27_tls.rs:68`).

Its three-state contract is the one the loop needs and is quoted here because
step (c) collapses if it is misread:

> `Some(Ok(true))` — readable (or errored/hung up, which a read then surfaces);
> `Some(Ok(false))` — the timeout expired. `None` means this build has **no**
> poll primitive at all … and is the signal for the caller to fall back to a
> plain blocking read rather than spin on a stub that answers "not ready"
> forever.

Answering the `None` arm with `continue` would compile, look exactly like the
other arms, and spin a loop that can never report readiness — the shape W7-53
refused to ship for the Windows pipe sink write. **`None` must break to one
plain blocking read.**

`TLS_CLOSE_POLL_MS = 25`, the value and role of `S2_CLOSE_POLL_MS`
(`servlet.rs:4501`), `NET_READ_CLOSE_POLL_MS` and `NET_WRITE_CLOSE_POLL_MS`
(`native-io/src/net.rs:2514`). Its expiry is **not an outcome** — it is only when
the registry is re-asked.

### 3.4 The carrier, and why it must be raised where it is

`ErrorKind::Interrupted` is this family's close carrier at all nineteen fixed
sites and at both `*_classify_after_block` functions
(`t27_tls.rs:4006`, `servlet.rs:2652`).

**Trap 1 is real and it is silent.** Both retry loops on this path swallow it:

* `http_url_connection::read_eof_tolerant` (`http_url_connection.rs:1355`) —
  `Err(e) if e.kind() == Interrupted || e.raw_os_error() == Some(4) => continue`;
* `EintrIo` (`native-io/src/eintr.rs`), used by the server read arm and both
  writes, whose whole documented purpose is that "no layer above it — rustls,
  `native_tls`, or a hand-written read loop — can observe the condition".

So a wakeup raised **inside** step (d) is retried into the pre-fix behaviour on
a code path that now *looks* close-aware. The loop above cannot make that
mistake because step (b) raises before any read is issued — but the reviewer's
checklist item is: **the `Interrupted` must originate above `read_eof_tolerant`,
never from within it.**

`rustls_classify_after_block` stays exactly as it is. It is the *other* half —
it turns a call that has already returned into the right Java answer — and it
must keep running on the (d) path, because a close that lands while the thread
is inside `complete_io` is still only observable after the return.

### 3.5 The deadline — trap 4, and the `SO_RCVTIMEO` rule

`t27_tls.rs:3193`/`:3194` and `:3356`/`:3357` set
`set_read_timeout(Some(30s))` / `set_write_timeout(Some(30s))` on every client
dial and accepted server socket. Two consequences:

1. **The row is masked today, not hanging.** A parked TLS read unwedges after
   ~30 s with a *timeout* error rather than a close classification. Wrong answer,
   bounded cost — which is why this row is correctly ordered behind the
   unbounded hangs W7-53 fixed first.
2. **A poll loop can turn that bound into a new hang.** A `recv` we no longer
   issue until the socket is ready is a `recv` `SO_RCVTIMEO` can never bound. The
   loop must therefore derive its own deadline from `raw.read_timeout()` and
   report `ErrorKind::TimedOut` when it expires — **derived, never invented**.
   `read_timeout()` answering `Ok(None)` means no deadline arm at all: a
   `SocketTimeoutException` on a socket with no timeout set is the regression
   this design is guarding against and is on the falsification list in §5.

This is the same per-regime rule W7-53 states and the same one `fd_table`'s four
sites and `async_socket::aio_read_close_aware` already implement.

### 3.6 EINTR

`poll(2)` is never restarted by `SA_RESTART`, and this VM sends `SIGUSR2` to
every thread for `jit::xt_root_scan`. `net_poll_raw` (`native-io/src/net.rs:2342`)
already reports EINTR as **not ready** rather than as an error, so the loop
inherits the correct behaviour: it re-asks the registry and re-polls with a fresh
slice, and it must **not** re-poll in place with the original `timeout_ms`, which
would restart the whole wait on every GC. Winsock has no EINTR and needs no arm.

---

## 4. What gets NO loop, and why that is a measurement rather than caution

* **`rustls_stream_write` and `s2_tls_write`.** Measured on HotSpot 25.0.3 /
  Windows 11, 2026-08-12 and recorded at `apps/probes/AsyncCloseProbe.java:1114`:
  `SSLSocket.close()` from another thread while a writer is parked inside
  `getOutputStream().write()` **does not return** — JSSE serialises
  `duplexCloseOutput()` behind the write. There is therefore no reference
  behaviour in which a close wakes a parked TLS write; a close-aware write loop
  here would not be parity, it would be a behaviour HotSpot does not have. The
  probe encodes this by accepting `CLOSE_BLOCKED` in `tlsWrite`'s `want` and
  failing on exactly one reading, `TIMEOUT`.
* **The server arm of `rustls_stream_read` and `s2_tls_read_direct`.** §0: their
  screen is `buffered_read_size()`, whose Windows implementation has a visible
  false-zero gap. They also carry `#[cfg(unix)] LegacyDsa(openssl::ssl::SslStream)`
  (`t27_tls.rs:1504`, `servlet.rs:2112`), which nothing on this host type-checks —
  writing its screen would be an API guess.
* **`rustls_server_accept` / `rustls_listener_close`.** Already close-aware by a
  different and correct mechanism: `TlsServerListenerEntry.closed: Arc<AtomicBool>`
  (`t27_tls.rs:1442`), set by `rustls_listener_close:4215` and polled by the
  non-blocking accept loop. Not part of this design.

---

## 5. How it is tested

### 5.1 The instrument exists and already has the rows

`apps/probes/AsyncCloseProbe.java`, with its HotSpot oracle transcript in
`apps/probes/AsyncCloseProbe.expected.txt`. W7-61 added three TLS rows on top of the
original thirteen: `tlsSelfTestNoPark` (`:997`), `tlsReadIntegrity` (`:1036`),
`tlsRead` (`:1090`), `tlsWrite` (`:1110`). The pilot needs no new probe.

### 5.2 How a row distinguishes "closed promptly" from "the peer happened to send"

This is the question the whole harness is built around, and it is answered by
**four independent structures, not one**:

1. **The park is proved before the stimulus.** The worker sets `row.entered`
   immediately before the blocking call; the driver waits for that flag, then
   settles, then asserts the worker has **not** returned. A row that returned
   inside that window is reported `INCONCLUSIVE` and can never be scored `PASS`
   (`AsyncCloseProbe.java:236`). So "the peer sent something" is caught *before*
   the close is issued: it makes the row inconclusive rather than green.
2. **The peer never writes.** `tlsRead` builds a `TlsPair`, hands the client end
   to the worker, and nothing on the accepted end ever calls `write`. The only
   event in the row's timeline after the park is the `close()`. There is no
   "happened to send" to confuse with a wakeup.
3. **`tlsSelfTestNoPark` is the negative control for exactly this confusion**
   (`:997`). It is the same TLS pair with the peer writing one byte and a 300 ms
   settle, and it **passes by not parking**. Without it, `parked=true` on
   `tlsRead` rests on a check nobody has seen refuse a TLS read — and a TLS read
   can fail to park for a reason unique to the record layer, which is the exact
   failure mode §0 describes. If `tlsSelfTestNoPark` ever reports
   `parked=true`, every other TLS row's verdict is void however green.
4. **The close is bounded and on its own thread, and a close that did not return
   is reported as its own state.** `AsyncCloseProbe.java:263`–`:301`: if the
   worker is still alive when the wait expires, the harness looks at the
   *closer* thread. Closer still alive ⇒ `CLOSE_BLOCKED` (a `PASS` only for a
   row whose `want` names it, `INCONCLUSIVE` otherwise) — "the stimulus was
   never delivered, so the row tested nothing". Closer finished and worker still
   parked ⇒ `TIMEOUT` ⇒ `FAIL`. That is the defect, and it is the only reading
   that is.

Two further guards that make the *pilot specifically* falsifiable:

* **`tlsReadIntegrity` (`:1036`) is the anti-vacuity row for the record layer.**
  A 200 KiB payload — many maximal 16 KiB fragments — written by the peer and
  compared byte-for-byte, with `CORRUPT@<offset>` naming the first wrong byte. A
  loop that woke a reader mid-record and handed back a partial buffer goes red
  here while `tlsRead` stays green. **This is the row that catches the screen
  being placed at the wrong level, and it is the one to watch when the pilot
  lands.**
* **The failure path has been executed.** `-Dprobe.skipClose=<row>` suppresses
  the close; measured on HotSpot 25.0.3 / Windows 11 2026-08-12, `socketRead`
  FAILs at `ms=4000` and the run exits 1. A harness whose failure path has never
  run is not known to have one.

### 5.3 What the pilot must additionally prove, and the row that does not exist yet

The deadlock §0 describes is **not** covered by any existing row: every TLS row
either parks with an empty pipe (`tlsRead`) or reads a payload the peer has
already fully written (`tlsReadIntegrity`, which is served largely from buffered
plaintext but whose writer is still running). The specific shape that would hang
is:

> peer writes a payload **larger than one `read` request** and then writes
> nothing more and does not close; the reader consumes it in several small
> `read`s. The second and later `read`s must be served from buffered plaintext
> **with no socket I/O**. A socket-readiness gate hangs on the second read.

**Add one row, `tlsReadBufferedRemainder`**, built exactly like the existing
twelve: peer writes 64 KiB once and then goes silent (no close, no further
write); reader issues `read(buf[1024])` in a loop; register it with
`expectPark == false` so a park is itself the failure, and give it `want =
returned:65536`. On the unsound design it reports `TIMEOUT`; on HotSpot and on
the screened design it returns. Without this row the pilot's central risk has no
gate at all, and the record's own standing rule — an assertion outside a
scheduled fixture cannot close a record — applies to it twice over.

### 5.4 Scheduling — the standing gap this design does not close

`apps/probes/AsyncCloseProbe.java` **has never run in any suite and cannot**:
`regression-suite/run.sh` compiles `"$HERE"/src/*.java` and runs a
hand-maintained word list, and reads nothing from `apps/probes/`. Moving it to
`regression-suite/src/RAsyncClose.java` and adding that name to `CORE_CLASSES`
(core, not `JDKONLY_CLASSES` — this family is a Compatible-mode defect strict
merely inherits) is a two-part edit in files this lane does not own. It is in
**NOMINATIONS**.

Two things to check when doing it: the probe calls `System.exit`, which the
harness's `PASS <Class>` grep tolerates but which must still emit the
`PASS RAsyncClose (N checks)` banner; and its `-Dprobe.*` properties must have
safe defaults, because the suite passes no `-D` of its own.

### 5.5 Run matrix

| arm | why |
|---|---|
| HotSpot 25 — **first, every time** | the control. A CratonVM row is evidence only after the same row has been seen to pass here. |
| `cratonvm.exe` (Compatible, `--real-jdk`) | the shipping arm; `NativeKind::Bridge`, so strict inherits it |
| `cratonvm.exe --jdk-only` | must agree with the above, row for row |
| `CRATONVM_REAL=-net-sockets` | routes the synthetic `java.net.Socket` surface, i.e. sites 3/4 |
| **Linux** | the pilot's Unix arm already gets its wakeup from W7-61's `raw` shutdown, so Linux must show **no change**. A Linux difference is a regression, not a win. |

Rust-side: `cargo test -p cratonvm-native-builtins --lib t27_tls::tests` and
`servlet::tests` must stay green, and `vm/tests/socket_input_stream_timeout.rs`
must be run because §3.5's deadline arm is exactly the kind of change that
regresses it.

---

## 6. What could still deadlock — the explicit list

Not residuals in general; specifically the ways this design, correctly
implemented, still fails to end a wait.

1. **Mid-record, and it is by construction.** `prepare_read`'s
   `while self.conn.wants_read()` loop calls `complete_io` repeatedly. A maximal
   TLS record is ~16 KiB and a TCP segment ~1.5 KiB, so a reader that passes the
   screen and enters (d) **parks inside `complete_io` between the segments of one
   record** and is unwakeable there. The design makes a reader close-aware while
   the record layer is **at rest** — the idle keep-alive connection waiting for
   the next request, which is where the hang matters — and leaves it unwakeable
   mid-record. That is the same statement as "do not abandon a read mid-record",
   priced. Anyone counting this row closed must count this with it.
2. **A `wants_read() == false` read that still blocks on a WRITE.** The screen
   is a read predicate, but `prepare_read` opens with `complete_prior_io`, which
   calls `complete_io` when `is_handshaking()` or `wants_write()`. So a read
   taken on the un-polled fast path can still park in `write_tls` if the peer's
   receive window is closed. Rare (a renegotiation or key update mid-stream) and
   bounded by `SO_SNDTIMEO`, but it is a park the screen does not cover and it
   must not be described as covered.
3. **A peer that stops mid-record forever.** (1) with the window widened: the
   reader is inside `complete_io` for as long as the peer withholds the rest of
   the record. Bounded only by the 30 s `SO_RCVTIMEO`.
4. **The write sites, deliberately.** §4. `tlsWrite`'s `CLOSE_BLOCKED` is the
   oracle's own behaviour; a close issued against a parked TLS write blocks on
   HotSpot too. This is a divergence we are choosing to keep.
5. **The other three sites, until their screen is measured.** §0. If someone
   copies the pilot to `s2_tls_read_direct` on the strength of
   `buffered_read_size()` alone, the schannel false-zero is a hang on every
   Windows Tomcat/WildFly TLS read, and the failure mode is *indistinguishable
   from the defect being fixed*.
6. **`poll_stream_readable` answering `None`.** Only on a build that is neither
   Windows nor Unix. Handled by breaking to one plain blocking read, i.e. the
   pre-fix behaviour, which is a lost wakeup and not a spin. Stated so nobody
   "fixes" it into `continue`.
7. **A second reader on the same stream.** Two Java threads reading one
   `SSLSocket` already serialise on L2 today; the pilot does not change that,
   and the screen is taken under L2 so it cannot be read stale. But the second
   reader waits on L2 while the first is inside (d), and L2 is not close-aware.
   The close still reaches the first reader via the registry re-ask, so the
   second is released transitively — one extra hop of latency, not a deadlock,
   but it is a wait no poll bounds.
8. **The `Arc<TcpStream>` keeps a descriptor alive past `close()`.** Intended —
   it is what retires trap 3 — but it means a parked reader holds one duplicate
   descriptor per parked read beyond the Java-visible close. Bounded by the
   number of parked readers, released when each retires. Not a leak; do check it
   under the Tomcat/WildFly load suites before calling the row closed.

---

## 7. Blast radius, and the argument for the pilot being one arm

`rustls_stream_read` and `s2_tls_read` carry Tomcat, WildFly, the Spring TLS
slices and H2-over-TLS. A screen that is wrong in the `false` direction is a
hang on **every TLS read in the VM**, and the failure mode is the same TIMEOUT
the fix exists to remove — which is why the pilot is one arm with a screen
proved exact from the dependency's own source, gated by a row
(`tlsReadIntegrity`) that goes red on precisely the mistake that is easy to make,
and why `tlsReadBufferedRemainder` has to be written before the pilot lands
rather than after.

The whole benefit is on **Windows**. On Unix, W7-61's `shutdown` on the
registry-held duplicate already delivers the wakeup, and this design's Linux
verdict is "no change" (§5.5). What the poll loop buys is that the reader is
parked in `WSAPoll` on a 25 ms slice instead of in `recv`, so the registry
re-ask can be reached at all on a platform where no `shutdown` aborts a pending
blocking call.

---

## NOMINATIONS

Out-of-file changes this design implies. None was made.

1. **`regression-suite/run.sh` + file move — schedule the instrument.**
   Move `apps/probes/AsyncCloseProbe.java` → `regression-suite/src/RAsyncClose.java`
   (renaming the class) and add `RAsyncClose` to `CORE_CLASSES`, not
   `JDKONLY_CLASSES`. Rationale and the two gotchas are in §5.4. Until this is
   done, no TLS async-close row can close a record.
2. **`apps/probes/AsyncCloseProbe.java` — add `tlsReadBufferedRemainder`.** §5.3. The
   pilot's central risk currently has no gate. Register it with
   `expectPark == false`, `want = "returned:65536"`.
3. **`native-builtins/src/t27_tls.rs` — the pilot itself.** §3. Specifically:
   `:1481` `raw: Option<TcpStream>` → `Option<Arc<TcpStream>>`; `:3263` and
   `:3683` `let raw = stream.sock.try_clone().ok();` →
   `let raw = stream.sock.try_clone().ok().map(Arc::new);`; the `for raw in [...]`
   at `:4170`–`:4178` split into two statements because the two halves no longer
   share a type; and the loop of §3.2 inserted between `:4028` and `:4030`.
4. **`native-builtins/src/servlet.rs`, `native-builtins/src/t27_tls.rs` (server
   arm) — NOT nominated.** §4 and §0. They must wait for a measurement of
   `buffered_read_size()`'s false-zero behaviour on schannel, and the
   `#[cfg(unix)] LegacyDsa` arm cannot be written from this host at all.
