# Fix note — `native-io-datagram` (S1: synthetic DatagramChannel shims)

Report item: `docs/reviews/fable-2026-06-10/native-io.md` → **S1 (policy violation)**.

## Problem

The `t16_dc_*` DatagramChannel family in `native-io/src/nio_native.rs` was a
**default-on synthetic shim** registered (via `register_t16_channel_overrides`,
called from `register_io_natives` in `lib.rs`) over the public
`java/nio/channels/DatagramChannel` methods `open / isOpen / isConnected /
isBlocking / configureBlocking / connect / disconnect / close`.

It fabricated datagram state — `t16_dc_connect` invents a `127.0.0.1:9` target
when the `SocketAddress` can't be decoded, **ignores the result** of the
underlying `udp.connect()` ("we still mark the channel as connected"), and
unconditionally flips the synthetic `connected` slot to 1. A Java caller thus
observes a *connected* `DatagramChannel` and a loopback target it never asked
for — exactly the fake app behavior the no-synthetic-stubs policy forbids.

These overrides were registered AFTER `datagram::register_datagram_real`
(lib.rs:3717) and so shadowed the real `sun.nio.ch.DatagramChannelImpl`
(`dgram_*0`) path on the public API methods, even though `datagram.rs` already
provides the real implementation.

## Fix

Gated the entire synthetic DatagramChannel surface behind the existing
`synthetic-jdk` cargo feature (off by default) and tagged it
`NativeKind::SyntheticStub`, mirroring how other synthetic families are gated in
this crate (`lib.rs` uses `#[cfg(feature = "synthetic-jdk")]` around its NIO /
ByteBuffer / StringReader overrides; `native-builtins/src/classfile_api.rs`,
`jmx.rs`, etc. tag synthetic registrations `NativeKind::SyntheticStub`).

In `native-io/src/nio_native.rs`:

1. **Functions** — added `#[cfg(feature = "synthetic-jdk")]` to all eight
   `t16_dc_open / t16_dc_is_open / t16_dc_is_connected / t16_dc_is_blocking /
   t16_dc_configure_blocking / t16_dc_connect / t16_dc_disconnect /
   t16_dc_close` definitions, plus a FLAGGED-SyntheticStub doc comment.

2. **UDP machinery** — the `udp_registry / udp_next_id / udp_register /
   udp_with / udp_remove` helpers and their imports
   (`parking_lot::RwLock`, `std::collections::HashMap`, `std::net::UdpSocket`,
   `std::sync::OnceLock`) are used *only* by the `t16_dc_*` family (verified:
   6 total occurrences of RwLock/HashMap/OnceLock, all in this block), so they
   are gated under the same `#[cfg(feature = "synthetic-jdk")]`. `ClassId` is
   left ungated (still used by `alloc_t16`, which the afc/asc/acg families
   share).

3. **Registration** — in `register_t16_channel_overrides`, the DatagramChannel
   `r.register(...)` block is wrapped in `#[cfg(feature = "synthetic-jdk")] { ...
   }`, sets the category to `NativeKind::SyntheticStub` for those eight entries,
   then restores `NativeKind::Bridge` for the remaining (logging / multicast /
   Net) registrations. The afc / asc / acg / logging / multicast / Net
   registrations are unchanged and remain in the default build.

## Effect

- **Default build:** the `java/nio/channels/DatagramChannel` public-API
  overrides are absent, so the real JDK `DatagramChannel` bytecode runs and
  dispatches to the real `sun.nio.ch` (`dgram_*0`) natives from `datagram.rs`
  (`register_datagram_real`, unchanged, ungated). No fabricated connect state.
- **`--features synthetic-jdk` build:** behavior is identical to before
  (synthetic shims compiled in, now tagged `SyntheticStub` so they show up
  correctly in `--dump-native-registry`).

## Safety / build notes

- All gated symbols are private `fn` (no `pub`); nothing outside the gated
  regions references them. `datagram.rs` only mentions `t16_dc_*`/`udp_registry`
  in comments and keeps its own parallel registry — no cross-module dependency.
- Imports/helpers/functions/registration all share the same single cfg, so both
  feature configurations compile as a consistent unit (no orphaned
  imports/functions in either config).
- Workspace lints set `dead_code`/`unused_imports` to `"allow"`, so even the
  belt-and-suspenders gating of the helpers cannot fail the build; gating them
  anyway keeps the default binary free of the unused UDP machinery.
- No new tests added: the existing `#[cfg(test)] mod tests` in this file covers
  only `range_len` and does not reference the `t16_dc_*` family, so gating does
  not affect the default test build. (Report noted the *fabricated-state*
  assertions live in the consuming `vm` crate integration tests, not here.)

## Files changed

- `native-io/src/nio_native.rs`
