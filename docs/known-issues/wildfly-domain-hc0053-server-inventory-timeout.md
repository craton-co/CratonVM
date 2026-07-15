# WildFly domain boot: `WFLYHC0053` inventory transport resolved; server-output reader residual remains

Status: OPEN — narrowed twice. The `WFLYHC0053` inventory timeout itself is fixed (2026-07-14), and the
StreamDecoder stale-buffer residual below is fixed (2026-07-15, `21c5d6f6`). What keeps this record open
is only the final "both managed servers reach WFLYSRV0025" bar: managed servers now launch, connect, and
register, but full server start was blocked during verification by the (since-fixed) STW/CHM boot wedges
tracked in `wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md` (2026-07-15
follow-up) and by shared-host load; see "2026-07-15 verification" below for exactly how far each probe
got.

## Fixed and removed from the active issue scope (2026-07-14)

The Process Controller emitted the correct WildFly protocol response: a `0x98` chunk header, payload beginning with the expected `0x15` inventory command, and `0x99` end marker. The Host Controller nevertheless read `0` for that one-byte command and rejected it as an invalid command, closed the connection, and later raised `WFLYHC0053`.

The fault was in `native_socket_input_stream_read_one` (`native-builtins/src/net_phase_e.rs`). It allocated a one-byte Java array, called the potentially blocking socket-read native, and then dereferenced its raw `ObjectRef`. A moving GC during the read could forward the array, leaving the local stale. The fix pins the one-byte array across the read, refreshes it through the pin, then reads and unpins it. The lower-level bulk read already uses the blocking-region/root protocol.

Two adjacent GC-safety defects exposed by the now-progressing boot were fixed in the same changeset:

- `native-collections` now pins and refreshes the values-view map/list around entry collection, and refreshes key/value references through resize and allocation in linked-hash-map insertion.
- Stream-chain processing pins its current element around lambda invocation and refreshes the forwarded value before continuing or emitting it.
- `DelegatingServiceController` no longer receives unsafe native aliases intended for the concrete MSC controller layout; the wrapper's inherited methods now dispatch normally.

With the unique remote binary `/data/bin/cratonvm-wildfly-hc0053-complete-20260714-231440`, a fresh WildFly 32.0.1.Final no-JIT domain probe passed the former handshake point, produced neither `Invalid command byte` nor `WFLYHC0053`, and launched both `Server:server-one` and `Server:server-two`.

## Fixed 2026-07-15: the StreamDecoder server-output reader residual

The previous "Remaining residual" — both launched servers' stderr-reader threads failing the
`CRATONVM_DBG_STALE_OBJREF` canary inside `java/io/InputStreamReader.read([CII)I` — is fixed on branch
`fix/wildfly-gc-pin-stream-20260715` (commit `21c5d6f6`, merged to dev with this doc update).
`native-io/src/stream_decoder.rs::decode_into` held three raw refs across its GC-capable refill window
(the temporary byte array across the potentially blocking `InputStream.read([BII)I` invoke; the
destination char array and `this` across the same window); `native_sd_read` re-used `this`+`out` across
`decode_into` iterations unpinned; `native_sd_close` re-used `this` after re-entering Java via
`close()`. All now pin and re-read through `native_pin_roots` per the Family-1 contract.

Verified: three no-JIT domain probes with `CRATONVM_DBG_STALE_OBJREF=1` (2026-07-15, fixed binary)
produced **zero canary firings anywhere in the process** — previously both server stderr-reader threads
tripped it. `cargo test -p cratonvm-native-io --lib`: 349 passed.

## 2026-07-15 verification state (why this record is still open)

Domain probes on the fixed binaries (Azure host, WildFly 32.0.1.Final, unique loopback/ports; logs under
`/data/wt-wfgc-20260715/probes/`):

- No `Invalid command byte`, no `WFLYHC0053`, no stale-canary firing in any probe (6+ runs, no-JIT and
  JIT, plain and canary-flagged).
- The Host Controller completes its own boot (`WFLYSRV0025 ... (Host Controller) started`), both managed
  server processes are spawned (`Starting process 'Server:server-one'`/`'server-two'`) and register
  (`WFLYHC0020: Registering server server-two`), and both emit `WFLYSRV0049 ... starting` on the console.
- Neither server reached its own `WFLYSRV0025 started` within the probe timeouts. Two boot-wide wedge
  mechanisms in exactly this window (a census-counted CHM segment-monitor deadlock against the STW
  barrier, and a segment-monitor ↔ registry-RWLock ordering cycle) were root-caused and fixed during
  this same session — see the 2026-07-15 follow-up in
  `wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`. The HC itself was measured
  stalling for minutes per STW pause pre-fix (`STW cross-thread JIT takeover ... rounds=64` in the HC
  log), which starved the server-registration sync; that marker is gone post-fix. The remaining
  verification gap is a clean post-fix domain run on a quiet host reaching `WFLYSRV0025` in both
  server logs.

## Verification completed for committed changes

- `cargo test -p cratonvm-native-builtins jboss_msc --lib`: 18 passed.
- `cargo test -p cratonvm-native-builtins net_phase_e --lib`: 36 passed.
- `cargo test -p cratonvm-native-collections --lib`: 72 passed.
- `cargo test -p cratonvm-native-io --lib`: 349 passed (2026-07-15).
- Remote fresh WildFly 32.0.1.Final domain probe reached managed-server launch and cleared the original protocol failure; 2026-07-15 probes additionally cleared the stale-canary bar and reached both-servers-registering.

## Next step

Re-run the no-JIT and JIT domain probes against a dev build containing `21c5d6f6` + `7831ce2c` +
`b1ac28f3` on a host with load ≤ cores, with generous (≥900 s) timeouts, and confirm
`domain/servers/server-{one,two}/log/server.log` each contain `WFLYSRV0025`. Everything else in this
record is fixed and verified.

## 2026-07-15 addendum: second stale site fixed live (toArray iterator); next blocker named (managed-server async-future stall)

Two of the backtrace-enabled stale-canary domain probes (DOM13/DOM17, logs under
`/data/wt-wfgc-20260715/probes/logs/` on the Azure host) caught a SECOND, unrelated stale-ref site in
the Host Controller during extension init: `real_jdk_to_array_typed` (`vm/src/vm/vm_init.rs`) — the
`toArray(T[])` iterator fallback re-used `this`/template/target/iterator raw across its repeated
GC-capable `ctx.invoke` calls (`size`/`iterator`/`hasNext`/`next`), and the ArrayList-shaped path read
`elementData` after the target allocation could move it. **FIXED** (`94145eaf`, pinned per the
Family-1 contract; `cargo test -p cratonvm-vm --lib`: 2217 passed). Other probes (DOM10/DOM16) ran the
identical config with zero firings — the site is timing-dependent, so future stale-canary runs should
always set `RUST_BACKTRACE=1`.

The remaining "both servers reach WFLYSRV0025" gap now has a concrete, named blocker: a live
sudo-gdb attach on a stalled `Server:server-one` process (`/data/tmp/server-one-stall.threads`) shows
its Controller Boot Thread parked in `monitor_wait` under
`async_future_wait_keepalive` (`native-builtins/src/wildfly_core.rs:1537`) — an async future that is
never completed — while two peer threads sit in interpreter `monitorenter` and every
XNIO/MSC/remoting carrier idles normally. The server processes reach `WFLYSRV0049 starting` +
root-service start, register with the HC (`WFLYHC0020`), then wedge there deterministically
(~210 console lines each, identical across probes and timeouts up to 1500 s, JIT and no-JIT alike).
This is a NEW, separate defect in the synthetic WildFly async-future/remoting sync — the next
investigation for this record, with the gdb dump above as its starting evidence.
