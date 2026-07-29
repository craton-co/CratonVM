# Open items behind the constant-valued native surface

**Status**: open. Split out of `docs/stub-census.md` when that file was retired
(2026-07-28) — the census itself is now enforced entirely by
`vm/tests/tier1_tests.rs::t9b_inline_constant_native_census`, which prints the
live counts and carries the triage guidance. What remained was this list of
real gaps, which belongs here under the normal known-issues convention rather
than in a counting document.

None of these is a stub. Each is a native that answers honestly today and
would answer *better* if the VM exposed data it already keeps, or if a
separate defect were fixed.

## Blocked on data the VM keeps but does not expose

- **JFR event writing.** `native-builtins` has no route into the VM's
  FlightRecorder — the entire JFR surface on `NativeContext` is one hard-coded
  `emit_virtual_thread_pinned_jfr`. That is why `emitEvent`, `commit`,
  `getStackTraceId` and the whole EventWriter path bottom out at constants.
  `FlightRecorder.isAvailable()` returning `true` is nonetheless correct: the
  lifecycle/clock surface it gates really is implemented, and JDK 25's
  `JVMSupport.checkAvailability` discards the value anyway.
  *Needs*: `NativeContext::jfr_emit(..)` / `jfr_is_recording()` in
  `native-api`, backed by the recorder `vm_exec.rs` already reaches.

- **Thread contention timing.** `isThreadContentionMonitoringSupported/Enabled`
  answer false because nothing records blocked/waited *durations* —
  `ThreadJmxSnapshot` has no such field. Distinct from lock OWNERSHIP, which
  is implemented.

- **`ThreadInfo.getLockedMonitors()` is contended-only.** The publish sites sit
  behind `monitors.enter_or_contend(..)` returning `Some`, so an uncontended
  acquisition is invisible, and `set_jmx_waiting_monitor` still has no caller
  so `Object.wait()` monitors are too. `isObjectMonitorUsageSupported()`
  therefore stays **false** on purpose: claiming support while returning a
  partial list is worse than reporting the feature unsupported. Deadlock
  detection is unaffected and does work. Completing it means recording every
  uncontended `monitorenter` — a hot-path registry write; HotSpot stack-walks
  at query time instead.

- **`URLClassLoader.close()`** cannot retract a dynamic-classpath entry:
  `NativeContext::register_dynamic_classpath` is append-only and returns no
  ids. *Needs*: per-path ids plus an `unregister_dynamic_classpath`.

- **`ForkJoinPool.awaitQuiescence`** returns `true`, which can lie: `execute`
  hands the Runnable to a real worker thread rather than running it inline.
  `false` would be worse — callers loop on it, so a wrong `false` is a hang.
  *Needs*: the pending-async-task count, which lives in `native-builtins` and
  is not visible from `native-collections`.

## Separate defects, not constants

- **`Charset.contains` throws `AbstractMethodError`** in real-JDK mode: the
  receiver resolves to the abstract `java/nio/charset/Charset` rather than a
  concrete `sun.nio.cs.*`. A class-identity bug.

- **`Files.getOwner` throws `UnsupportedOperationException`** where HotSpot
  answers, including on Windows.

- **`java/net/http/HttpClient` carriers exist in three incompatible shapes**
  (9, 10 and 1 slots) across `net_phase_e.rs`, `http2.rs` and
  `net_channels.rs`, with `newHttpClient`/`newBuilder`/`version` registered in
  all three plus `servlet.rs`. A live last-registration-wins hazard.

- **`ResultSetMetaData` and the JDBC surface are synthetic-jdk only.** The
  real-JDK twin (`native-builtins/src/jdbc.rs`) covers driver discovery and
  SQL date/time but not metadata, so the wave-5 column-type work does not
  apply in the default build.

- **`Document.getElementById` needs a parser feature**: `XmlParser::skip_prolog`
  discards the DTD internal subset, so no `<!ATTLIST .. ID>` survives and the
  id table is unconditionally empty.

## Platform coverage, not stubs

- **Windows host-interface enumeration** needs `GetAdaptersAddresses`. The
  `NetworkInterface` implementation reads `/sys/class/net`, so on Windows
  `getAll()`/MAC/MTU degrade to loopback-only. Same for the Linux-only
  `TCP_KEEPIDLE` / `TCP_QUICKACK` / `IP_DONTFRAGMENT` socket options.

## Design decision, deliberately not changed

- **`System.setSecurityManager`** now enforces
  `RuntimePermission("setSecurityManager")`, but CratonVM still *models* an
  installable SecurityManager at all — which JDK 24 (JEP 486) permanently
  disabled. Adopting JEP 486 would disable CratonVM's own `Runtime.exec` and
  Panama gating, so it is a decision about the security model rather than a
  stub to remove.

## OPEN: the synthetic field-table trap has not been audited systematically

A `classloading::synthetic_stub_fields` entry that is **too short fails
silently**. `set_field` past the end of an object DISCARDS the write rather
than erroring, so the natives look implemented and store nothing.

**Five confirmed instances, all fixed 2026-07-28** — the rate is the point,
which is why this needs an audit rather than fixing them as they surface:

| class | declared | needed | effect of the gap |
|---|---|---|---|
| `HttpExchange` | 8 | 9 (now 11) | authenticated principal lost |
| `HttpServer` | 5 | 6 | `setExecutor` discarded |
| `sun/net/httpserver/HttpServerImpl` | *absent* (table keyed `com/...`) | 6 | wrong package entirely |
| `java/net/DatagramSocket` | *absent* | 4 | whole phase-72 set inert, incl. the ctor's fd |
| `DatagramPacket`, `Preferences` | *absent* | 5 | every raw-slot native inert |

**Who is exposed**: only classes whose instances are created by **bytecode
`new`**, since that path sizes the object from this table. Call sites using
`alloc_concurrent_synthetic(ctx, name, n)` pass an explicit count and take
`max(requested, real)`, so they are safe — of 504 such sites, 368 name a class
with no table entry, and that is FINE. Do not read that number as a bug count.

**What has NOT been done**: nobody has enumerated the classes that are both
`new`-instantiated from Java AND read by raw slot index from a native. That
intersection is where any remaining instances live. Until then the rule is:
**add the slot to `synthetic_stub_fields` in the same change as the constant in
the native file**, and prefer `set_field_by_name` or an ObjectRef-keyed side
table (as `net_phase_e::register_re7_datagram_socket` does) over raw slot
indices on a class you do not allocate yourself.

## OPEN: phase-72 DatagramSocket shadows a working implementation

`phases_late::net_channels`'s phase-72 `java/net/DatagramSocket` set (slot
based) registers *after* `net_phase_e::register_re7_datagram_socket` (side
table based), so it wins in `--synthetic-jdk` builds for every overlapping key
(`<init>`, `send`, `close`, `isClosed`, `getLocalPort`, `get/setSoTimeout`,
`get/setReuseAddress`) — the broken set beating the working one. The field
entry above makes the slot-based set functional, but the duplication remains
and the two can drift. The StampedLock precedent applies: pick which registrar
owns the class rather than leaving both.

## OPEN: the phase-72 fixes are LATENT — synthetic-jdk only, unverified

`DatagramSocket.connect/disconnect`, `DatagramChannel.disconnect` and the
`HttpExchange.getLocalAddress/getRemoteAddress` getters were implemented
2026-07-29, but they live in `phases_late::net_channels`'s phase-72 registrar,
which is reached only from `register_synthetic_overrides` —
`#[cfg(feature = "synthetic-jdk")]`, a non-default feature. Measured before and
after against HotSpot 25 in the DEFAULT build, the probe output is byte
identical:

    http.exchange.localAddress   THREW:AbstractMethodError   (both)
    http.exchange.remoteAddress  THREW:AbstractMethodError   (both)
    udp.connect.then.disconnect  THREW:InternalError         (both)

In the default build there is no native for these at all, so resolution lands
on the abstract declaration. The producer half (`net_phase_e
::re10_dispatch_pending` capturing the accepted socket's addresses) IS
real-JDK-live; only the consumer half is gated. **Anyone finishing this needs
to decide whether the phase-72 HTTP/datagram surface belongs on the live path,
or whether the real-JDK path needs its own registrations.**

Consequence for reviewers: these implementations are type-checked and reasoned
but NOT behaviourally verified. Do not treat them as proven.

## OPEN: `StackFrame.getDescriptor()` still throws (fix did not land)

`StackWalker.getInstance(RETAIN_CLASS_REFERENCE).walk(.. getDescriptor())`
throws `UnsupportedOperationException` where HotSpot 25 returns
`()Ljava/lang/Object;`. `expandStackFrameInfo` was implemented on 2026-07-29
specifically to fix this by filling the frame's `type` slot lazily — and the
before/after probe is unchanged, so the exception originates somewhere other
than the path that was changed. The `expandStackFrameInfo` work is still
correct in itself; it is simply not what this call reaches. Next step is to
find the actual thrower rather than to re-implement expansion.

Related and also unchanged: `StackWalker.getInstance()` WITHOUT
`RETAIN_CLASS_REFERENCE` still allows `getDeclaringClass()` where HotSpot
throws. `ClassFrameInfo.ensureRetainClassRefEnabled()` now implements the check
honestly and `populate_sfi` propagates the walker's flag, but the shadowing
`getDeclaringClass()` native deliberately does not consult it (matching the
documented choice for the sibling carrier in `phases_late::reflect_invoke`).
