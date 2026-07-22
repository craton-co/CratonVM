# Tomcat NIO Selector — open investigation

## Status

**RESOLVED 2026-06-05 (branch `fix/tomcat-nio-selector`).** Apache Tomcat
10.1.31 now **serves real HTTP** under CratonVM: `GET /` returns
`HTTP/1.1 404` (chunked) — a genuine response from Coyote/Catalina (404
because no ROOT context is mapped to `/`; the HTTP request/response cycle
itself works end-to-end). Boot is clean: **0** `ClosedSelectorException`
(was 23,622 per boot), **0** "server channel not bound", no error storm.

This took NINE distinct fixes, each exposed by fixing the previous one
(the NIO path never reached the next stage before). See
"## RESOLUTION" at the bottom for the full chain. The original
speculation below (WindowsSelectorImpl native surface) was the WRONG
root cause — the real bugs were a synthetic-object field-layout collision
plus a memory-model divergence and a reentrant mutex deadlock.

---

<details><summary>Original (2026-05-28) investigation — superseded</summary>

After `cdcf159` (real-JDK `InetSocketAddress` holder layout
in NIO bind), Tomcat 10.1.31 reaches `Server startup in [156-361] ms`
with the server socket successfully bound to port 8080. However, the
NioEndpoint poller's selector loop fires continuously with:

```
ERROR [org.apache.tomcat.util.net.NioEndpoint] Error in selector loop (java/io/IOException: ClosedSelectorException)
```

Until this is resolved Tomcat is up but does not accept connections.

## Diagnosis

Added `CRATONVM_DBG_SEL=1` env-gated `eprintln` instrumentation to **all
three** of CratonVM's `java/nio/channels/Selector.open()` native
registrations:

- `servlet.rs::register_s2_selector` (5-field synthetic, S2SEL layout)
- `phases_late.rs::register_p98_*` (4-field synthetic, slot-indexed)
- `tests_extracted.rs::register_*` (5-field synthetic, S2SEL layout)

**During a 25-35 s Tomcat boot, ZERO of these traces fire.** Tomcat's
poller is creating its `Selector` instance through a code path that
*does not* dispatch through our static `Selector.open()` native.

The likely chain (consistent with what bypassed our `RandomAccessFile`
natives during the dacapo-luindex investigation):

```
Tomcat NioEndpoint.Poller.<init>:
    selector = Selector.open();
        ↓
    bytecode of java/nio/channels/Selector.open():
        return SelectorProvider.provider().openSelector();
            ↓
        SelectorProvider.provider():
            (real-JDK static — singleton; on Windows returns
             sun.nio.ch.WindowsSelectorProvider)
            ↓
        WindowsSelectorProvider.openSelector():
            return new WindowsSelectorImpl(this);
                ↓
            WindowsSelectorImpl.<init>:
                wakeupSourceFd = native pipe()  ← throws here, or
                ... = native call ...           ← here, with bogus state
```

The resulting `WindowsSelectorImpl` instance carries CratonVM-allocated
slots that look right (its `<init>` and field setters all run), but the
underlying native pipe / Windows-specific fd handles never get
established because their `WindowsSelectorImpl.poll0` / `Net.pipe0` /
similar natives are not registered. The poller's later
`selector.select(timeout)` enters `WindowsSelectorImpl.doSelect` which
checks `if (closed) throw new ClosedSelectorException()` — the
"closed" bit is set by `AbstractSelector.<clinit>` or by an earlier
swallowed `IOException` in the constructor, so every subsequent
`select` throws the same exception.

Without `[SEL]` traces firing, we can't yet pinpoint which native call
returns the bad state — but the symptom (continuous
`ClosedSelectorException` from a selector our `open()` native never
created) is conclusive.

## Re-application paths, in increasing order of work

1. **Intercept `SelectorProvider.provider()`.** Return a synthetic
   `SelectorProvider` whose `openSelector()` calls our existing
   synthetic Selector path. ~30 lines, plus matching synthetic class
   layout for `SelectorProvider`. Risk: anything else that expects to
   see the real provider (e.g. `inheritedChannel()`,
   `openDatagramChannel()`) breaks unless we cover those methods too.

2. **Make `Selector.open()` static native always win.** The native is
   registered but doesn't fire — investigate why. Possible causes:
   - `java/nio/channels/Selector` is loaded with a different class
     loader than the one our registration is keyed on.
   - The bytecode for `Selector.open()` is short enough that the
     interpreter inlines it as `SelectorProvider.provider().openSelector()`
     without consulting the native registry.
   - There's an `<clinit>`-time intercept that runs before our
     registration phase.
   Confirming requires adding a trace to the native dispatch table
   lookup itself, not just to the registered native.

3. **Implement the real-JDK `WindowsSelectorImpl` / `WEPollSelectorImpl`
   native surface.** `poll0`, `setupPipe0`, `interrupt0`, `wakeup0` plus
   the platform-conditional `getAcceptCount`/etc. This is the path
   HotSpot and OpenJDK take; it's the most correct but the most work
   (estimate: ~500 lines of native registrations plus a fd registry
   for the wakeup pipes).

## What stays in tree after this investigation

- `CRATONVM_DBG_SEL=1` env-gated traces in `servlet.rs` and
  `phases_late.rs` Selector natives. Costs one `var_os` check per
  call when the env var is unset; the steady-state cost is zero.
- This doc.

The actual fix for the selector loop is **out of scope for this
session** — it needs the dispatch-layer investigation in path (2) above
before a path is chosen.

## How to reproduce

```
TOMCAT=C:/craton/CratonVM/test-infra/regression-pool/apps/apache-tomcat-10.1.31
cd $TOMCAT
CRATONVM_DBG_SEL=1 timeout 30 \
    C:/craton/CratonVM/target/release/cratonvm.exe \
    --java-home "C:/Program Files/Java/jdk-25" -Xmx512m \
    -Dcatalina.home=$(pwd) -Dcatalina.base=$(pwd) \
    -cp "bin/bootstrap.jar;bin/tomcat-juli.jar" \
    org.apache.catalina.startup.Bootstrap 2>&1 | tail -30
```

Expected: `Server startup in [...] ms` line, then `ERROR [Acceptor]
Socket accept failed` once, then continuous `ERROR [NioEndpoint] Error
in selector loop (java/io/IOException: ClosedSelectorException)` until
the process is killed.

No `[SEL]` lines appear in stderr, confirming the natives are
bypassed.

## Related fixes

- `cdcf159` (2026-05-28) — `s2_parse_socket_addr` /
  `p98_extract_socket_addr` handle real-JDK `InetSocketAddress` holder
  layout. This is what unblocked Tomcat from STARTING_PREP; the
  selector gap surfaces *because* the bind is now working.
- `3c288b0` (2026-05-28) — removed synthetic
  `Connector.startInternal` / `AbstractProtocol.start` stubs that
  short-circuited the real Tomcat lifecycle. Without those removed
  the selector error would never have been observable.
- `e2031a2` (2026-05-28) — same shape of real-JDK-layout audit for
  `RandomAccessFile.getFD` / `File.path`. The Lucene `FSDirectory.sync`
  failure documented in `bc-ec-mod-mododdinverse-investigation.md` and
  this Tomcat Selector issue likely share the same dispatch-layer root
  cause; whichever is investigated first will unblock the other.

</details>

---

## RESOLUTION (2026-06-05) — Tomcat serves HTTP

The 2028-05-28 theory (missing `WindowsSelectorImpl` natives) was wrong.
CratonVM has a real platform-backed selector (`native-io/src/nio_selector.rs`,
WSAPoll on Windows). The actual blockers, in the order they surfaced
(each fix exposed the next — the NIO path simply never reached the next
stage before):

**Root cause class 1 — synthetic-object field-layout collision.** CratonVM
now loads the REAL JDK abstract classes (`sun.nio.ch.SelectorImpl`,
`java.nio.channels.ServerSocketChannel/SocketChannel`). The synthetic NIO
natives stored int state (selector id/open flag, channel registry id,
local port, …) in low object slots that collide with real reference-typed
fields (`selectorOpen`, `closeLock`, `provider`, `keyLock`, …). Writing an
`Int` into an `L…;`-typed slot is descriptor-coerced to `null` on BOTH read
and write (`gc::coerce_field_value_by_descriptor`), so the state never
persisted.

1. **Selector (Bug B — the `ClosedSelectorException` storm).**
   `selector_open_native` allocated a real `SelectorImpl` and wrote
   `SI_OPEN_FLAG`(slot 4)/`SI_ID`(slot 0) → coerced to null → `open_flag`
   read false → every `select()` threw `ClosedSelectorException` (23,622×).
   FIX: key the selector object to its native id by GC-stable identity hash
   (`sel_obj_ids` side-table, the existing C27 pattern); openness lives in
   the native `SelectorState.open`.
2. **ServerSocketChannel/SocketChannel (Bug A — "server channel not bound").**
   Same collision for `F_REG_ID`(2)/`F_LOCAL_PORT`(4)/… → bind never
   recorded the listener id → `accept()` threw "server channel not bound";
   `getLocalAddress()` was null. FIX: an identity-hash `chan_fields`
   side-table (`cf_get`/`cf_set`); remote host kept as a Rust `String` so no
   un-rooted Java ref goes stale under a moving GC. `channel_net_fd` exposes
   the id to the selector's register path.

**Downstream natives (exposed once accept worked):**

3. **`SocketChannel.socket()` (Bug C)** — abstract method, no native →
   `AbstractMethodError` in `NioEndpoint.setSocketOptions`. Added an
   `sc_socket` adapter returning a bare `java.net.Socket`.
4. **Non-blocking accept clone mode (Bug D)** — `ssc_accept` cloned the
   listener but the clone didn't inherit non-blocking mode on Windows →
   `accept()` blocked forever. Set the clone's mode explicitly.
5. **`register` on concrete channel classes (Bug E)** — native dispatch
   (WP0.1) keys on the receiver's concrete class; the public 3-arg
   `register(sel, ops, att)` lives on `AbstractSelectableChannel` and its
   real bytecode touches uninitialized `regLock`/`validOps`. Registered the
   override on `SocketChannel`/`ServerSocketChannel`(+Impl).
6. **Socket option setters (Bug F)** — `SocketProperties.setProperties`
   calls `setReceiveBufferSize`/`setKeepAlive`/`setTcpNoDelay`/… on the
   adapter; the real bytecode calls `getImpl()` → NPE (no impl). No-op'd
   them on `java/net/Socket` (gate-aware: dropped under
   `CRATONVM_REAL_NET_SOCKETS`).
7. **`getRemoteAddress`/`getLocalAddress` (Bug G)** — needed by
   `NioSocketWrapper.populateRemoteAddr`. Built a real `InetSocketAddress`
   via `new_object_initialized`.

**Root cause class 2 — off-heap memory-model divergence (Bug H, GENERAL).**
A `DirectByteBuffer` from `ByteBuffer.allocateDirect` is backed by a real
`dbb_allocate` pointer. But CratonVM's `Unsafe` accessed it inconsistently:
   - The 5-arg `Unsafe.copyMemory` (used by `DirectByteBuffer.put/get(byte[])`)
     had TWO registrations; the **heap↔heap-only** `native_unsafe_copy_memory`
     won and silently **dropped** mixed heap↔off-heap copies. FIX: point all
     5-arg `copyMemory`/`copyMemory0` at `native_unsafe_copy_memory_consolidated`
     (a strict superset that routes the off-heap side through
     `copy_*_native_memory`).
   - The 3-arg `Unsafe.getByte/putByte(Object,long)` with a **null base**
     (byte-wise `DirectByteBuffer` access — the HTTP parser) stashed bytes in
     a `static_int_store` HashMap, diverging from the raw/arena memory the
     socket layer reads. FIX: dedicated `native_unsafe_get/put_byte_mb`
     handlers that route a null base through `copy_*_native_memory` (same
     arena-or-raw path the socket/FileChannel I/O uses).

   This was a PRE-EXISTING general bug (any NIO direct-buffer socket I/O
   round-tripped as zeros); it only surfaced now that the NIO path reached
   read/write. Verified: `NioEcho`/`NioEchoMT` (single- + multi-threaded
   Poller pattern, direct buffers) round-trip PING/PONG; `DbbProbe` byte-wise
   still passes.

**Root cause class 3 — reentrant mutex deadlock (Bug I — the final one).**
`sk_set_interest_ops` (public `SelectionKey.interestOps(int)`) held the
per-selector `parking_lot::Mutex` (`sel.lock()`) and then called
`selector_set_interest`, which re-locks the SAME mutex (non-reentrant) →
the Poller thread wedged forever inside `NioEndpoint.unreg()`. The trace
showed `select → n=1`, `selectedKeys → 1`, `readyOps` (×2, isReadable +
unreg mask), then silence — no `sc_read`, no error, client read-timeout.
FIX: resolve `(selector id, net fd)` and set `interest_ops` under the lock,
then DROP it before the OS-level `selector_set_interest` update.

### How it was diagnosed

- Isolated Java probes against the live binary (no rebuilds) reproduced each
  bug: `NioTomcatProbe`/`NioStep` (selector + bind/accept), `NioEcho`/
  `NioEchoMT` (full Poller pattern, single + multi thread), `DbbProbe`
  (direct-buffer address scheme), `TcExec` (Tomcat's custom
  `ThreadPoolExecutor` — confirmed working, ruling the executor out).
- `CRATONVM_DBG_SCBUF` traces proved `copy_*` wrote PING to raw memory while
  Java read zeros → the `static_int_store` divergence.
- `CRATONVM_DBG_NIO` traces (register/select/selectedKeys/attachment/readyOps/
  sc_read/sc_write) localized the deadlock: everything worked up to `readyOps`
  in `unreg`, then the Poller hung. (Traces were removed after diagnosis.)

### Verified

- `GET /` → `HTTP/1.1 404` (chunked) — real response, request read + response
  written (sc_read "GET / HTTP/1.1", sc_write "HTTP/1.1 404").
- Boot: 0 ClosedSelectorException, 0 "not bound", clean.
- Regression pool: see commit (the `Unsafe` byte/copyMemory changes are
  general — the pool's `directbuffer-nio`/`unsafe-mem`/`filechannel-rt`
  probes gate them).

### Still open (separate, NOT NIO)

- ROOT webapp returns 404 (no context mapped to `/`) — webapp deployment /
  JSP is a separate concern, not the NIO connector.
- `CRATONVM_REAL_NET_SOCKETS` stays default-OFF (the synthetic
  `java.net.Socket`/NIO-channel surface is what serves; the real
  `sun/nio/ch/Net` migration is a separate effort).
