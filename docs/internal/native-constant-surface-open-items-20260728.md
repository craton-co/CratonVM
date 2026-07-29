# Native constant-surface residuals — fixed 2026-07-29

This document was opened on 2026-07-28 after `docs/stub-census.md` was
retired.  It is now an internal completion record: every identified native
surface and its follow-up residual has a live implementation or an explicit,
intentional platform/design boundary.

## Completed work

- JFR event writing now reaches the VM recorder through `NativeContext`,
  including EventWriter/commit, stack-trace ids, recording state, and dump
  output.
- Thread JMX records contention/wait durations and monitor ownership for both
  uncontended monitorenter and `Object.wait`, so the support flags and returned
  `ThreadInfo` data agree.
- `URLClassLoader.close()` now uses receiver-local close state consistently for
  class, resource, and resource-enumeration lookup.
- `ForkJoinPool.awaitQuiescence` observes the live asynchronous-task count
  until the supplied deadline instead of returning a fabricated success.
- Real-JDK registrations cover `Charset.contains`, `Files.getOwner`, JDBC
  `ResultSetMetaData`, `StackFrame.getDescriptor`, retained-class-reference
  enforcement, and the HTTP client carrier shape.
- JFR, JMX, DOM ID lookup, HTTP exchange endpoints, `DatagramChannel`
  connectivity, and `DatagramSocket.connect`/`disconnect` are all routed to
  their live implementations.
- `URLClassLoader.close`, `DatagramSocket`, and `DatagramChannel` avoid raw
  real-JDK field assumptions by using side tables where appropriate.
- Windows now uses `GetAdaptersAddresses`; Windows extended socket options use
  Winsock for `IP_DONTFRAGMENT` and the supported TCP keepalive controls.
  `cratonvm-native-io` cross-compiles for `x86_64-pc-windows-gnu`.

## Synthetic layout and registration closure

`synthetic_stub_fields` now reserves the required positional capacity for the
issue-surface bytecode-`new` classes: `DatagramSocket`, `DatagramPacket`,
`Preferences`, both `HttpServerImpl` spellings, `HttpServer`, `HttpExchange`,
and `StackWalker$StackFrame`.  The regression test
`native_constant_surface_raw_slot_layout_audit` prevents those layouts from
becoming silently short again.

Phase 72 used to overwrite the moving-GC-safe side-table `DatagramSocket`
registrar because the latter was installed too early.  It is now registered
after phase 72, making it the final owner for every overlapping native key.
The synthetic-JDK `DatagramSocket.connect`/`disconnect` probe passes with both
JIT and `--nojit`.

The comprehensive probe fixtures are real-JDK fixtures.  They intentionally
use APIs absent from the synthetic class library (for example
`MethodType.descriptorString`, `BasicFileAttributes.owner`, and URL-based HTTP
server construction), so those missing synthetic declarations are not
interpreted as failures of the corresponding real-JDK native surfaces.

## Validation

On Azure `/data`, using the isolated
`fix/native-constant-surface-close-20260729-019fae0a` worktree and unique
binaries:

- Full real-JDK issue probe suite passed in JIT and `--nojit`: JFR event/dump,
  JMX contention and monitors, URLClassLoader close, ForkJoin quiescence,
  charset/files owner, HTTP client carrier, HTTP exchange addresses, JDBC
  metadata, DOM ID lookup, stack frame descriptor/retain behavior,
  DatagramSocket, and DatagramChannel.
- Synthetic-JDK build passed; its shared DatagramSocket lifecycle operation
  passed under JIT and `--nojit` after final registrar ordering.
- `cargo test -p cratonvm-classloading
  native_constant_surface_raw_slot_layout_audit --lib` passed.
- `cargo check -p cratonvm-cli` passed.
- `cargo check -p cratonvm-native-io --target x86_64-pc-windows-gnu` passed.

## Intentional security-model boundary

`System.setSecurityManager` remains installable under CratonVM's permission
model.  JDK 24's JEP 486 model deliberately disables it, but adopting that
model here would remove CratonVM's `Runtime.exec` and Panama permission gates.
That is a security-model decision, not an unresolved native stub or a
constant-valued answer.
