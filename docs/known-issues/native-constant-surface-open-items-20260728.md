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

## A trap worth keeping in mind

A synthetic field-table entry that is **too short fails silently**.
`set_field` past the end of an object DROPS the write rather than erroring, so
the native looks implemented while storing nothing. `HttpExchange` declared 8
slots while the code used 9 (losing the authenticated principal);
`DatagramPacket` and `Preferences` had no entry at all and allocated zero-slot
objects. If you add a slot constant in a native file, add it to
`classloading::synthetic_stub_fields` in the same change.
