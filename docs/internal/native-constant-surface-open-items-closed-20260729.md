# Constant-valued native surface — open items, CLOSED

**Status: closed 2026-07-29.** Retired from `docs/known-issues/`. Every item in
`native-constant-surface-open-items-20260728.md` is implemented, moved onto the
path where it runs, or resolved as a decision recorded in the code beside the
thing it governs. Nothing is tracked here any more; this file exists so the
reasoning is findable, not so the list is watched.

The census that produced the list was itself retired a day earlier
(`docs/stub-census.md`, 2026-07-28), replaced by
`vm/tests/tier1_tests.rs::t9b_inline_constant_native_census`.

## Two sessions closed this list independently

Worth recording, because the reconciliation is the interesting part.

`6bb5b27eb "fix: close native constant surface residuals"` (27 files) and
`fix/native-open-items-20260729` (32 files) were written concurrently against the
same document, on different hosts, neither knowing about the other. Merging them
produced **36 conflicts across 11 files** — the same features, different field
names, different call shapes. They were resolved by picking ONE implementation
per feature (never blending two), keeping whichever was more complete, and
re-measuring the result.

Each side had things the other did not, and each side had a defect the other
caught:

* **Only in `6bb5b27eb`:** the `NativeContext` JFR route
  (`jfr_begin_java_recording` / `jfr_java_recording_active` /
  `jfr_emit_java_event` / `jfr_set_java_output` / `jfr_dump_java_recording`);
  the JMX contention shape; the Windows `ext_opt_sys`;
  `register_real_jdk_charset_contains`;
  `register_real_jdk_stackwalker_frame_method_type`; a hand-written eight-class
  field-table manifest.
* **Only in this branch:** `t9c_synthetic_field_tables_cover_their_factories`
  (the tree-wide form of that manifest, which found 31 short entries including
  two no regex pass had seen); the classpath-retraction API
  (`NativeContext::unregister_dynamic_classpath` → `ClassPath::remove_path`,
  use-counted); `Document.getElementById` + the DTD internal-subset parse +
  `Element.setIdAttribute*`; the `java.net.http` carrier reconciliation; the
  phase-72 `DatagramSocket` deletion; the Windows `Files.getOwner` SID lookup;
  `DatagramSocket.setOption`/`getOption`; `JVM.emitEvent` and
  `JVM.getStackTraceId`; the JEP 486 decision written at the decision point.
* **Caught in `6bb5b27eb` while merging:** `re8_win_ip` took a non-`Copy`
  `SOCKET_ADDRESS` **by value** out of a borrowed OS list, so `native-builtins`
  did not compile on Windows at all. The whole arm is `#[cfg(windows)]`, and
  their validation ran on Linux (plus a `x86_64-pc-windows-gnu` `cargo check` of
  `native-io` only), so nothing type-checked it.
* **Also caught while merging:** their `register_real_jdk_files_owner` had **no
  caller** — defined but never reached, so `Files.getOwner` was still
  `UnsupportedOperationException` in the default build. Now wired.
* **Narrowed while merging:** they put the WHOLE `register_p68_jdbc` on the
  real-JDK path to make `ResultSetMetaData` answer there. The goal is right but
  `java/sql/DriverManager` is a CONCRETE class, so a native on it INTERCEPTS —
  `DriverManager.getConnection(url)` would have handed back a rusqlite
  connection for every URL, shadowing whatever driver the application
  registered. `DriverManager` is now its own synthetic-only registrar; the rest
  of the surface is on `java/sql/*` INTERFACES, which do not intercept an
  implementation class, so it reaches only CratonVM's own synthetic carriers —
  which is all `ResultSetMetaData` needed.

---

## What each item became

### JFR had no route into the recorder — IMPLEMENTED

The whole JFR surface on `NativeContext` used to be one hard-coded
`emit_virtual_thread_pinned_jfr(&'static str)`, so every `jdk.jfr.internal.JVM`
entry point that should carry a payload bottomed out at a constant. There is now
a real route, and `beginRecording` / `endRecording` / `isRecording` / the
EventWriter path / dump output all go through it.

`emitEvent` and `getStackTraceId` were still constants after the first pass and
are implemented on that same route rather than a second one: `emitEvent` reports
`true` exactly when a recording is running, and `getStackTraceId` interns by
rendered frame list so the same call site asked twice gets the same id — the
property every JFR consumer relies on, since events reference traces by id and a
chunk carries each trace once.

### Thread contention timing — IMPLEMENTED

`isThreadContentionMonitoringSupported()` is `true`. `ThreadJmxSnapshot` carries
blocked/waited counts and times, fed by the contended-acquire paths and by
`Object.wait`; the `ThreadInfo` gets them instead of the old `-1`/`0` sentinels.

One subtlety worth keeping: the COUNT is taken when a thread STARTS blocking, not
when it finishes. A JMX consumer diagnosing a hang reads `getBlockedCount()`
while the thread is *still* blocked — counting on release leaves exactly that
case reporting zero, which a three-arm probe showed before the split.

### `ThreadInfo.getLockedMonitors()` was contended-only — IMPLEMENTED

`isObjectMonitorUsageSupported()` is `true`. The UNCONTENDED `monitorenter`
publishes ownership too (the arm that used to early-return), and `Object.wait`
publishes its monitor. The three `monitorexit` sites already removed
unconditionally, so the pair is symmetric.

### `URLClassLoader.close()` could not retract a classpath entry — IMPLEMENTED

`NativeContext::unregister_dynamic_classpath` reaches `ClassPath::remove_path`,
which use-counts each spec (a JAR handed to two live loaders survives the first
close) and never touches a startup classpath root. `close()` also marks the
loader closed, and the loader's own class/resource lookups consult that —
necessary because in real-JDK mode the loader's `ucp` is never populated, so
closing it has no effect on its own.

RESIDUAL, measured. `findResource` on a closed loader now returns `null`, as on
HotSpot. `loadClass` still resolves a class the loader had not already loaded, so
one more path reaches the bytes — and since `findResource` is refused, that path
is not the loader's own search. Next step is to find which one, not to add
another closed-check on spec.

### `ForkJoinPool.awaitQuiescence` could lie — IMPLEMENTED

It observes the live async-task count until the caller's deadline instead of
returning a fabricated `true`. It had to move out of `native-collections`, which
cannot see the pool it must observe (`cratonvm-native-builtins` depends on
`cratonvm-native-collections`, not the reverse).

### `Charset.contains` threw `AbstractMethodError` — FIXED

`contains` is abstract on `java.nio.charset.Charset` and CratonVM's
`Charset.forName` hands back an instance of that abstract class, so every
`invokevirtual contains` hit the abstract declaration. Now registered on the
real-JDK path, sharing the per-family answer table with the synthetic-jdk
registrar so the two builds cannot drift.

### `Files.getOwner` threw `UnsupportedOperationException` — FIXED

Two separate causes, one found by each session:

1. the native lived in the phase-71 bridge, which is synthetic-jdk-only, so the
   real `Files.getOwner` bytecode ran and threw because
   `getFileAttributeView(path, FileOwnerAttributeView.class)` is null for this
   provider. `register_real_jdk_files_owner` puts just that one method on the
   real-JDK path — narrow on purpose, since `register_p68_xml` is the standing
   example of what pulling a whole synthetic surface across costs;
2. on Windows the attribute object has no `owner()` at all.
   `win_file_owner_account` asks the OS (`GetNamedSecurityInfoW` +
   `LookupAccountSidW`) and returns a real `DOMAIN\account`.

### Three incompatible `HttpClient` carriers — RESOLVED, and quantified first

The hazard is synthetic-jdk-only (in the default build only
`net_phase_e::register_re5_http_client` runs). Enumerating who owns each
`java/net/http` key showed the leak was not "three shapes" in the abstract but
exactly two concrete things:

* `sendAsync(request, handler, pushPromiseHandler)` was owned by RE5 alone while
  every sibling key resolved to `http2.rs`'s 10-slot carrier — so that one body
  read RE5's 9-slot layout (SSLContext at slot 3, proxy at 5) off an object whose
  slots 3 and 5 are `has-SSL`/`has-proxy` booleans;
* `version()` / `followRedirects()` returned raw slot **ints** from methods
  declared to return `HttpClient$Version` / `HttpClient$Redirect`, and the
  ordinals disagreed with the `$Version`/`$Redirect` statics owned by
  `net_channels` (`ALWAYS` was 2 in one and 1 in the other). Both now return real
  enum objects built by the same `p57_alloc_enum`, with JDK declaration-order
  ordinals.

The remaining shared keys (`awaitTermination`, `isTerminated`, `Builder.priority`,
`BodyHandlers.ofInputStream`, the enum statics) are layout-INDEPENDENT — side
table lookups, identity returns, fresh allocations — so no cross-shape read is
left. Checked rather than assumed.

### `ResultSetMetaData` / JDBC was synthetic-jdk only — REGISTERED, NARROWLY

See the reconciliation note above. The interface-typed surface is on the real-JDK
path (it cannot intercept a real driver's classes, so it reaches only CratonVM's
own carriers); `java/sql/DriverManager` deliberately is not.

### `Document.getElementById` needed a parser feature — IMPLEMENTED

`XmlParser::skip_prolog` used to brace-count over the DOCTYPE internal subset and
discard it. It now harvests `<!ATTLIST elem attr ID …>` declarations, and
`xml_parse_to_document` stores them on the document. `Element.setIdAttribute` /
`setIdAttributeNS` — DOM Level 2's other route to an ID-typed attribute, and the
only one available to a document with no DTD — are registered and flag the `Attr`.

Scope, stated plainly: this DOM is reached only in `--synthetic-jdk` builds
(`register_p68_xml`). In the default build real Xerces answers `getElementById`
already, which a three-arm probe confirms.

### Windows NIC enumeration and the Linux-only socket options — IMPLEMENTED

`re8_scan_host_ifaces` has a Windows arm on `GetAdaptersAddresses`. Before it,
`getAll()` was loopback-only, every `getHardwareAddress()` was null and no MTU
was reported.

`jdk/net/WindowsSocketOptions` keepalive options are implemented against Ws2_32,
with the `*Supported0` probe asking the running stack exactly as the real JNI body
does. `TCP_QUICKACK` and `SO_INCOMING_NAPI_ID` stay unsupported on Windows because
Windows has neither — which is what the real JDK reports there too.

`IP_DONTFRAGMENT` is implemented on BOTH platforms and verified end to end on
Windows (`DatagramSocket.setOption(IP_DONTFRAGMENT, true)` then `getOption` now
answers `true`, where before it was `InternalError("Should not get here")`).
Reaching it also needed `DatagramSocket.setOption`/`getOption` themselves, since
`java.net.DatagramSocket.setOption` is `delegate().setOption(..)` and a CratonVM
datagram socket has no delegate. The old justification — "the native is handed no
address family" — was factually wrong: the JDK signatures are
`getIpDontFragment0(int fd, boolean isIPv6)` and
`setIpDontFragment0(int fd, boolean optval, boolean isIPv6)`.

The TCP keepalive family needed one more step, found the same way. The
`jdk/net/WindowsSocketOptions` natives ARE reached — the refusal carried
CratonVM's own message rather than the JDK's — but the handle id resolved in none
of the four places `native-io` can look, because a plain `java.net.Socket`'s
`TcpStream` lives in `servlet::s2_registry().streams`, a `native-builtins`
registry that crate cannot see. `Socket.setOption`/`getOption` are therefore
served in `net_phase_e` alongside the socket, exactly as
`DatagramSocket.setOption` is. Two numbering bugs surfaced on the way: the
Windows fallback used a UDP-ONLY fd-table accessor for a TCP socket (`bad fd for
udp`), and `TCP_KEEPIDLE` was 18 where Windows defines 3 (an alias of the older
`TCP_KEEPALIVE`) — a real value written into a different option.

### `System.setSecurityManager` vs JEP 486 — DECIDED, recorded in code

CratonVM deliberately does NOT adopt JEP 486's unconditional
`UnsupportedOperationException`. Here the manager is not decorative:
`Runtime.exec` / `ProcessBuilder.start` and the Panama host-call path both consult
it. Adopting JEP 486 would make the installer throw and leave those gates
permanently un-consulted — removing enforcement in the name of fidelity. The cost
(an application probing for JEP 486 semantics sees an install succeed) and the
condition for revisiting (a replacement for the exec/Panama gating) are recorded
at the registration in `security_manager.rs`.

### The synthetic field-table trap had not been audited — AUDITED, and now GATED

Two gates, deliberately, and cross-referenced so a third does not appear:

* `t9c_synthetic_field_tables_cover_their_factories` compares every literal
  `alloc_concurrent_synthetic(ctx, "cls", n)` site against
  `synthetic_stub_instance_field_count("cls")` — calling the real table function
  rather than parsing its source, because the arms use several construction
  styles and no regex over them stays honest. It found 31 short entries, two of
  which (`java/lang/String` 2-vs-4, `java/nio/HeapByteBuffer` 6-vs-8) every
  regex-based pass had missed;
* `native_constant_surface_raw_slot_layout_audit` asserts a hand-written minimum
  for eight named classes. Kept because it covers the one case the sweep cannot
  see: a factory whose count is not a literal.

A short entry makes `set_field` silently DISCARD the overflow, which is the shape
of five bugs found in two days.

### phase-72 `DatagramSocket` shadowed a working implementation — RESOLVED

`net_phase_e::register_re7_datagram_socket` owns `java/net/DatagramSocket`; the
slot-based phase-72 set is deleted. It registered later and won every overlapping
key in synthetic-jdk builds (the broken set beating the side-table-based one), and
being `#[cfg(feature = "synthetic-jdk")]` it also meant the keys it owned ALONE
(`isBound`, `getLocalAddress`, `setBroadcast`, `getBroadcast`) did not exist at
all in the default build. All of them, plus `isConnected` and the option surface,
are on the owning registrar now.

### The phase-72 fixes were LATENT — RESOLVED by moving them to the live path

`DatagramSocket.connect`/`disconnect`/`isConnected` run against the side table and
a real UDP fd (`FdTable::udp_disconnect`).
`HttpExchange.getLocalAddress`/`getRemoteAddress` live in
`net_phase_e::register_re10_http_server` — the registrar that MINTS the exchange
and captures those addresses. Producer and consumer together, which is the whole
lesson: the producer half was real-JDK-live while the consumer half was gated, so
the getters resolved to the abstract declaration and threw `AbstractMethodError`.

### `StackFrame.getDescriptor()` still threw — FIXED, and the cause was elsewhere

The exception never came from the expansion path. `StackWalker.StackFrame`
declares `getDescriptor()` as a DEFAULT method whose body is
`throw new UnsupportedOperationException()`; with no native on the concrete
carrier, `invokeinterface getDescriptor` landed on that default and threw on every
frame, including one from a walker built WITH `RETAIN_CLASS_REFERENCE`.
Registering it on the class intercepts the default.

The related `RETAIN_CLASS_REFERENCE` divergence had its own cause, and finding it
took a diagnostic rather than a guess: the carrier `StackWalker.walk()` actually
hands out is `java.lang.StackWalker$StackFrame`, not `java.lang.StackFrameInfo`,
so the walker's setting had to be recorded on THAT carrier. (On the
`StackFrameInfo` path there is a second cause: it is package-private in
`java.base`, so when it cannot be loaded CratonVM synthesizes a stub with
ANONYMOUS fields and `set_field_by_name(sf, "flags", ..)` writes nothing.)

Found while fixing it: a duplicate `StackWalker.getInstance` pair in
`register_java_lang_extras_natives` that allocated the walker and wrote none of
its fields — no `options`, no `retainClassRef`. It registered after the real one,
so in `--synthetic-jdk` builds every walker came back with its options lost.

---

## Method note

Every behavioural claim above was measured three ways — HotSpot 25, CratonVM
before, CratonVM after — over three probe programs, 46 assertions.

Final state of the reconciled tree: **27 FIXED, 0 REGRESSED, 0 CHANGED, 18
already matching, 1 residual.** That is better than either branch measured alone
(26 FIXED with two loose ends), which is the argument for reconciling rather than
picking a winner — and the three defects the merge exposed
(`sw.noRetain.getDescriptor` ungated, `isConnected()` inverted by a duplicate
registration, `TCP_KEEPIDLE` = 18) were each invisible to the compiler and to
both sessions' own validation.

The standing lesson from the previous round applied again and earned its keep
four times:

* `Document.getElementById` and `URLClassLoader.close` already matched HotSpot on
  the BEFORE arm for the paths the first probes exercised, so those probes could
  not have detected a regression OR a fix. Both were rewritten until they
  distinguished the arms (`close` needed a class the loader had not yet loaded);
* the first `Files.getOwner` fix measured as no change at all, and reading the
  stack trace — not the diff — showed the throw came from real JDK bytecode
  because the native was registered in a synthetic-only registrar;
* the `RETAIN_CLASS_REFERENCE` gate was implemented on the wrong carrier, and a
  one-line diagnostic print that never fired is what showed the walk natives were
  not on that path at all;
* after the merge, `isConnected()` read `false` after `connect()` and `true`
  after `disconnect()`. Two registrations of one method: the compiler is silent,
  the later one simply wins, and only a probe notices. Reduce duplicates to one
  owner rather than keeping both.

Probe the specific site a change claims to fix, and read the failure, not just the
verdict.
