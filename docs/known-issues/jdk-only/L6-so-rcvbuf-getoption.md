# `SO_RCVBUF` read back as 0 — `sun/nio/ch/Net.getIntOption0` never asked the OS

**Status:** FIXED (code landed 2026-08-06, unverified — no binary was built in
the session that wrote it). Lane L6 of the jdk-wave2 pool.

## The failure

`regression-suite/src/RJdkNet.java` fails in **both** `--real-jdk` and
`--jdk-only`; HotSpot 25 passes it (exit 0, `PASS RJdkNet (58 checks)`).

```
CK RJdkNet dns loopback=127.0.0.1 v6len=16 uri=/a/b
Exception in thread "main" java/lang/AssertionError: SO_RCVBUF is positive
    at RJdkNet.main(RJdkNet.java:308)
    at RJdkNet.loopbackTcp(RJdkNet.java:180)
    at RJdkNet.check(RJdkNet.java:43)
```

`RJdkNet.java:180` is `check(s.getReceiveBufferSize() > 0, "SO_RCVBUF is
positive")`, where `s` is a `new Socket()` that has been *created* but never
bound or connected — the option block at lines 169-185 runs entirely on an
unconnected socket. We answered `0`.

This is an ordinary Compatible-mode defect; the fix is mode-independent and
lives on the real-JDK bytecode path both modes share.

## Root cause

Both modes run real `java.net.Socket` bytecode here, so the call goes

```
Socket.getReceiveBufferSize()
  -> NioSocketImpl.getOption(SocketOptions.SO_RCVBUF)
  -> sun.nio.ch.Net.getSocketOption(fd, family, SO_RCVBUF)
  -> sun.nio.ch.Net.getIntOption0(fd, mayNeedConversion, level, opt)   <-- our native
```

`native-io/src/net.rs::net_get_int_option0` answered live socket state for
**one** option and a remembered request for everything else:

```rust
    if let Some(s) = stream_handle {
        if level == IPPROTO_TCP && opt == TCP_NODELAY {
            let v = s.nodelay().unwrap_or(false);
            return Ok(Some(Value::Int(if v { 1 } else { 0 })));
        }
    }
    // Otherwise return what was last set, defaulting to 0.
    let v = net_opts().read().get(&(fd, level, opt)).copied().unwrap_or(0);
```

Nothing had ever called `setReceiveBufferSize`, so the `(fd, level, opt)` lookup
missed and the `unwrap_or(0)` produced the failing `0`. There is no live socket
to fall back to either: `net_socket0` (net.rs, "The OS socket is not created
yet") registers `NetSocketHandle::Unbound` and the real `TcpStream`/`TcpListener`
only appears at `connect0` / `bind0`.

The rest of the option block passes *because* of the same cache — every one of
`setTcpNoDelay`/`setKeepAlive`/`setReuseAddress`/`setSoLinger` round-trips
through `net_opts` without the kernel being involved at all. `getTcpNoDelay()`
returning `true` at line 171 on a socket whose fd has no OS socket behind it is
the tell: the value came from the request record, not from a socket.

Two further consequences of the same shape, fixed here as well:

* The constants `SOL_SOCKET = 1` / `SO_KEEPALIVE = 9` compared against in
  `net_set_int_option0` are the **Linux** numbering. `getIntOption0`/
  `setIntOption0` are handed the platform's own pair — OpenJDK generates
  `sun.nio.ch.SocketOptionRegistry` per target OS, so on Windows `SOL_SOCKET`
  arrives as `0xFFFF` and `SO_KEEPALIVE` as `0x0008` and neither ever matched.
* An option set before `connect`/`bind` (the documented JDK ordering for
  `setReuseAddress` / `setReceiveBufferSize`) was recorded and then never
  applied to the socket that later appeared.

## What changed — all in `native-io/src/net.rs`

1. **New `sockopt_sys` module** — raw `getsockopt`/`setsockopt` via `libc`
   (unix) and `#[link(name = "ws2_32")]` FFI (windows), in the same
   dependency-free style as this crate's existing `WSAPoll` / `ioctlsocket`
   shims (`socket2` is not in `native-io`'s dependency tree). The Windows extern
   signatures are byte-identical to the `ext_opt_sys` block below it, because
   `clashing_extern_declarations` is `deny` workspace-wide.
2. **`net_getsockopt_int` / `net_setsockopt_int`** reproduce `Net.c`'s argument
   shapes: `SO_LINGER` is a `struct linger` (collapsed to `l_onoff ? l_linger :
   -1` on read, `l_onoff = arg >= 0` on write), the two IPv4 multicast options
   are a `u_char` on Unix, everything else is an `int`. The `(level, optname)`
   pair is passed through untranslated, which is what makes it correct on both
   platforms.
3. **`net_get_int_option0` now answers in three steps**: the live socket first
   (real `getsockopt`), then the recorded request, then
   `net_default_int_option(level, opt)` — a memoised probe of a throwaway
   loopback socket, which is the value HotSpot's own fd would report for a
   socket nothing has configured. That last step is what turns the failing `0`
   into the kernel default (65536 on Windows, 131072 on Linux). It also corrects
   `SO_LINGER` on a fresh socket from `0` ("linger on, 0 s") to `-1` ("off").
4. **`net_set_int_option0` applies for real** when there is a socket to apply to
   — the `std`-typed `set_nodelay` remains as the fallback for handle shapes the
   raw path cannot address. Failures are logged (`CRATONVM_DBG_NET=1`), never
   raised: an option the platform refuses used to be dropped silently, and
   failing the caller's `setOption` would be a worse regression.
   This retires the CONTRACT-DRIFT note of 2026-05-24 that made `SO_KEEPALIVE` a
   silent no-op for want of a `socket2` dependency.
5. **`net_apply_recorded_options(fd)`** is called from `net_connect0` and
   `net_bind0` so options set while the fd was `Unbound` reach the kernel the
   moment a socket exists. Without this, step 3 above would make a pre-connect
   `setKeepAlive(true)` flip from `true` to `false` at connect time.
6. **`net_opts` is re-keyed** fd -> `(level, opt)` -> value, so the replay costs
   O(options on this fd) instead of a scan of every socket the process has
   opened, and `close_net_fd` can drop the whole set in one `remove`. Entries
   were never removed before — the map grew unbounded for the life of the VM.

A listener's `Arc<Mutex<TcpListener>>` is taken with `try_lock`, never `lock`:
`net_accept` holds that mutex for the whole (unbounded) accept, so a blocking
lock here would stall an option query on an idle `ServerSocket` until a client
happened to connect. A failed `try_lock` falls through to the recorded value.

## Verifying once a binary exists

```
# the failing vector, both modes; HotSpot is the oracle
cratonvm.exe --real-jdk -cp regression-suite/classes RJdkNet
cratonvm.exe --jdk-only -cp regression-suite/classes RJdkNet
# expected on both: "PASS RJdkNet (58 checks)", exit 0

# the unit tests added alongside the fix
cargo test -p cratonvm-native-io --lib net::tests

# to watch the option path decide
CRATONVM_DBG_NET=1 cratonvm.exe --jdk-only -cp regression-suite/classes RJdkNet
#   [NET] getIntOption0 fd=0x4000000N level=65535 opt=4098 -> 65536 (live)
#   [NET] default level=65535 opt=4098 -> Some(65536)
```

The three unit tests added to `net.rs`'s `mod tests`:
`net_opts_are_dropped_when_the_socket_closes` (the leak), and
`so_rcvbuf_default_is_positive_without_a_live_socket` (the assertion this bug
report is about, at the Rust level — it fails on an unfixed tree because
`net_default_int_option` does not exist there).

## What would falsify the diagnosis

`CRATONVM_DBG_NET=1` showing `getIntOption0` never firing for `level`/`opt`
= the platform's `(SOL_SOCKET, SO_RCVBUF)` during `RJdkNet.loopbackTcp`. That
would mean some other registration is intercepting `getReceiveBufferSize`
ahead of the real-JDK bytecode — `native-builtins/src/net_phase_e.rs:4483`
registers exactly that method on `java/net/Socket` with an 8192 fallback, and if
*it* were the one answering, the assertion could not have seen a 0 in the first
place.
