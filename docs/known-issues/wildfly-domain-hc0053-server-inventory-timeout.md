# WildFly domain boot: `WFLYHC0053` inventory transport resolved; server-output reader residual remains

Status: OPEN — the `WFLYHC0053` inventory timeout itself is fixed. This record remains open only for the newly exposed moving-GC stale-reference residual described below.

## Fixed and removed from the active issue scope (2026-07-14)

The Process Controller emitted the correct WildFly protocol response: a `0x98` chunk header, payload beginning with the expected `0x15` inventory command, and `0x99` end marker. The Host Controller nevertheless read `0` for that one-byte command and rejected it as an invalid command, closed the connection, and later raised `WFLYHC0053`.

The fault was in `native_socket_input_stream_read_one` (`native-builtins/src/net_phase_e.rs`). It allocated a one-byte Java array, called the potentially blocking socket-read native, and then dereferenced its raw `ObjectRef`. A moving GC during the read could forward the array, leaving the local stale. The fix pins the one-byte array across the read, refreshes it through the pin, then reads and unpins it. The lower-level bulk read already uses the blocking-region/root protocol.

Two adjacent GC-safety defects exposed by the now-progressing boot were fixed in the same changeset:

- `native-collections` now pins and refreshes the values-view map/list around entry collection, and refreshes key/value references through resize and allocation in linked-hash-map insertion.
- Stream-chain processing pins its current element around lambda invocation and refreshes the forwarded value before continuing or emitting it.
- `DelegatingServiceController` no longer receives unsafe native aliases intended for the concrete MSC controller layout; the wrapper's inherited methods now dispatch normally.

With the unique remote binary `/data/bin/cratonvm-wildfly-hc0053-complete-20260714-231440`, a fresh WildFly 32.0.1.Final no-JIT domain probe passed the former handshake point, produced neither `Invalid command byte` nor `WFLYHC0053`, and launched both `Server:server-one` and `Server:server-two`.

## Remaining residual (not fixed in this checkpoint)

With `CRATONVM_DBG_STALE_OBJREF=1`, both launched server stderr-reader threads later fail the stale-reference canary while executing `java/io/InputStreamReader.read([CII)I`. The reported object is an array. Real-mode `InputStreamReader` delegates to `sun.nio.cs.StreamDecoder`; `native-io/src/stream_decoder.rs::decode_into` allocates a temporary byte array, invokes `InputStream.read([BII)I` (which may trigger a moving GC), then reads the temporary array through its pre-GC raw reference. This is the next concrete defect to fix: pin and refresh that temporary buffer, and also preserve any destination array used after the delegate call, then repeat clean no-JIT and JIT domain boots.

This residual is distinct from the resolved Process Controller inventory defect. It must remain under investigation until the stale-reference canary and clean repeated probes both pass.

## Verification completed for committed changes

- `cargo test -p cratonvm-native-builtins jboss_msc --lib`: 18 passed.
- `cargo test -p cratonvm-native-builtins net_phase_e --lib`: 36 passed.
- `cargo test -p cratonvm-native-collections --lib`: 72 passed.
- Remote fresh WildFly 32.0.1.Final domain probe reached managed-server launch and cleared the original protocol failure.

## Next step

Fix the StreamDecoder temporary-buffer lifetime described above, rebuild a uniquely named binary, and require at least one stale-canary no-JIT run plus clean no-JIT and JIT domain boots with both managed servers started before resolving this record.
