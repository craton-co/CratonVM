# Constant-valued native surface — open items, CLOSED

**Status: closed 2026-07-29.** Retired from `docs/known-issues/`. Every item in
`native-constant-surface-open-items-20260728.md` was either implemented, moved
onto the path where it runs, or resolved as a decision recorded in the code
beside the thing it governs. Nothing is tracked here any more; this file exists
so the reasoning is findable, not so the list is watched.

The census that produced the list was itself retired a day earlier
(`docs/stub-census.md`, 2026-07-28), replaced by
`vm/tests/tier1_tests.rs::t9b_inline_constant_native_census`. This round adds a
second gate, `t9c_synthetic_field_tables_cover_their_factories`, for the failure
mode that kept recurring.

---

## What each item became

### JFR had no route into the recorder — IMPLEMENTED

The whole JFR surface on `NativeContext` used to be one hard-coded
`emit_virtual_thread_pinned_jfr(&'static str)`, so every `jdk.jfr.internal.JVM`
entry point that should carry a payload bottomed out at a constant. There are now
four: `jfr_is_recording`, `jfr_set_recording`, `jfr_emit(event_type, fields)` and
`jfr_stack_trace_id` (`native-api/src/registry.rs`), implemented in
`vm/src/vm/vm_exec.rs` against the `FlightRecorder` the VM already owned.

`beginRecording`/`endRecording`/`isRecording` now start, stop and read a real
recording; `emitEvent` emits and reports whether it was stored;
`getStackTraceId` interns by frame list so the same call site gets a stable id.

What is still constant, and why, is now a different statement: `flush`,
`markChunkFinal`, `emitOldObjectSamples`, `emitDataLoss` and the `set*` buffer
knobs address a **chunk writer** this VM does not have. That is a feature this VM
lacks, named at each registration, not a missing route.

### Thread contention timing — IMPLEMENTED

`isThreadContentionMonitoringSupported()` is `true`. `ThreadJmxSnapshot` carries
`blocked_count` / `blocked_time_ms` / `waited_count` / `waited_time_ms`, fed by
the two contended-acquire paths in `vm_exec` and by `monitor_wait`;
`alloc_snapshot_thread_info` writes them into the `ThreadInfo` instead of the old
`-1`/`0` sentinels.

Counts accumulate always (one relaxed add per contended acquisition). The two
TIMES accumulate only while `setThreadContentionMonitoringEnabled(true)` is in
effect and read back as the JMM's `-1` sentinel otherwise — which is the
specified behaviour, and the state HotSpot boots in.

### `ThreadInfo.getLockedMonitors()` was contended-only — IMPLEMENTED

`isObjectMonitorUsageSupported()` is `true`. Both gaps are closed:

* the UNCONTENDED `monitorenter` publishes ownership too. Both acquire paths call
  `complete_jmx_monitor_enter` on the `enter_or_contend(..) == None` arm — the arm
  that used to early-return. The three `monitorexit` sites already removed
  unconditionally, so the pair is symmetric and the cost is one registry write
  the exit path has always paid;
* `set_jmx_waiting_monitor` / `take_jmx_waiting_monitor` are called around
  `monitor_wait`, so a thread inside `Object.wait()` reports its monitor instead
  of nothing.

### `URLClassLoader.close()` could not retract a classpath entry — IMPLEMENTED

`NativeContext::unregister_dynamic_classpath` reaches
`ClassPath::remove_path`, which use-counts each spec (a JAR handed to two live
loaders survives the first close) and never touches a startup classpath root.
`close()` retracts the loader's own URLs; classes already defined stay defined,
matching HotSpot, where `close()` shuts the `URLClassPath` and unloads nothing.

The gap the item named — "the native ABI is append-only, so a native cannot name,
let alone drop, the entries a loader added" — is closed. `close()` also had to be
registered on the real-JDK path with a layout-free body
(`servlet::register_url_classloader_close_bridge`, reached from `vm_init`): the
two existing implementations both read the URL array out of a fixed slot and
write a `closed` flag into another, which is safe only on the SYNTHETIC carrier.

RESIDUAL, measured and not papered over. A three-arm probe shows a closed loader
in real-JDK mode still resolving a class it had not already loaded, and
`findResource` still returning a URL. The cause is a second mechanism, not the
retraction: in real-JDK mode CratonVM never populates the loader's `ucp`, so the
searches are served by `classloader_real::ucl_real_find_class` and
`classloader::ucl_find_resource` instead — both now consult a closed-loader set
(`ucl_mark_closed` / `ucl_is_closed`, keyed by identity hash so a moving GC
cannot stale the key), and the probe should be re-run against those. If it still
resolves, the next thing to check is whether the real-JDK `close()` bridge is the
registration that wins for a real `java.net.URLClassLoader` receiver — measure
`findResource` first, since it is the shortest path from `close()` to an
observable.

### `ForkJoinPool.awaitQuiescence` could lie — IMPLEMENTED

Moved from `native-collections` (which cannot see the pool it must observe) to
`native-builtins::register_forkjoin_quiescence`, installed by `vm_init`
immediately after `register_concurrent_natives` so it is the registration that
wins. It polls the async worker pool's active count and queue depth until
quiescent or the caller's timeout expires. The old `true` remains in
`native-collections` as the fallback for an embedding that registers only that
crate, with the reason spelled out there.

### `Charset.contains` threw `AbstractMethodError` — FIXED

`contains` is abstract on `java.nio.charset.Charset` and CratonVM's
`Charset.forName` hands back an instance of that abstract class, so every
`invokevirtual contains` hit the abstract declaration. Now registered in the
real-JDK registrar, sharing the per-family answer table with the synthetic-jdk
one so the two builds cannot drift.

### `Files.getOwner` threw `UnsupportedOperationException` — FIXED

Two separate causes:

1. the native lived in the phase-71 bridge, which is synthetic-jdk-only, so the
   real `Files.getOwner` bytecode ran and threw because
   `getFileAttributeView(path, FileOwnerAttributeView.class)` is null for this
   provider. `register_files_owner_bridge` puts just that one method on the
   real-JDK path (narrow on purpose — the phase-68 XML umbrella is the standing
   example of what pulling a whole synthetic surface across costs);
2. on Windows the attribute object has no `owner()` at all. `win_file_owner_account`
   asks the OS (`GetNamedSecurityInfoW` + `LookupAccountSidW`) and returns a real
   `DOMAIN\account`.

### Three incompatible `HttpClient` carriers — RESOLVED, and quantified first

The hazard is synthetic-jdk-only (in the default build only
`net_phase_e::register_re5_http_client` runs). Enumerating who owns each
`java/net/http` key showed the leak was not "three shapes" in the abstract but
exactly two concrete things:

* `sendAsync(request, handler, pushPromiseHandler)` was owned by RE5 alone while
  every sibling key resolved to `http2.rs`'s 10-slot carrier — so that one body
  read RE5's 9-slot layout (SSLContext at slot 3, proxy at 5) off an object whose
  slots 3 and 5 are `has-SSL`/`has-proxy` booleans. `http2.rs` now covers the key;
* `version()` / `followRedirects()` returned raw slot **ints** from methods
  declared to return `HttpClient$Version` / `HttpClient$Redirect`, and the
  ordinals disagreed with the `$Version`/`$Redirect` statics owned by
  `net_channels` (`ALWAYS` was 2 here and 1 there). Both now return real enum
  objects built by the same `p57_alloc_enum`, with JDK declaration-order ordinals.

The remaining shared keys (`awaitTermination`, `isTerminated`, `Builder.priority`,
`BodyHandlers.ofInputStream`, the enum statics) are layout-INDEPENDENT — side
table lookups, identity returns, fresh allocations — so no cross-shape read is
left. That is the whole of the hazard, checked rather than assumed.

### `ResultSetMetaData` / JDBC is synthetic-jdk only — CORRECT, recorded in code

Not a gap. In synthetic-jdk mode `java.sql.*` has no bytecode, so the
rusqlite-backed surface IS the provider; in real-JDK mode the APPLICATION's driver
supplies the implementation classes, and registering these natives there would
shadow the driver's own metadata with SQLite's answers about a database it may not
be talking to. The precedent is `register_p68_xml`, which broke Tomcat's
`server.xml` parsing when it was put on the real-JDK path. What real-JDK mode
genuinely needs — driver discovery, SQL date/time conversion — is in
`native-builtins/src/jdbc.rs`, which IS registered there. The reasoning now lives
on `register_p68_jdbc`.

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

`re8_scan_host_ifaces` has a Windows arm built on `GetAdaptersAddresses`, with
the JDK's own per-`IfType` naming (`eth%d` / `lo%d` / `ppp%d` / `tun%d` /
`net%d`) so names round-trip through `getByName`. Before it, `getAll()` was
loopback-only, every `getHardwareAddress()` was null and no MTU was reported.

`jdk/net/WindowsSocketOptions` keepalive options are implemented against Ws2_32
`setsockopt`/`getsockopt`, with the `*Supported0` probe asking the running stack
exactly as the real JNI body does. `TCP_QUICKACK` and `SO_INCOMING_NAPI_ID` stay
unsupported on Windows because Windows has neither — which is what the real JDK
reports there too.

`IP_DONTFRAGMENT` is implemented on BOTH platforms. The old justification —
"the native is handed no address family" — was factually wrong: the JDK signatures
are `getIpDontFragment0(int fd, boolean isIPv6)` and
`setIpDontFragment0(int fd, boolean optval, boolean isIPv6)`.

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

`t9c_synthetic_field_tables_cover_their_factories` compares every literal
`alloc_concurrent_synthetic(ctx, "a/b/C", N)` site against
`synthetic_stub_instance_field_count("a/b/C")` — calling the real table function
rather than parsing its source, because the arms use several construction styles
and no regex over them stays honest. Both halves are exact, so a failure is a real
disagreement rather than a scanner artifact.

It found 31 short entries, all fixed (`pad_to(..)` preserves named slots and their
indices and appends anonymous ones). Two of them — `java/lang/String` 2-vs-4 and
`java/nio/HeapByteBuffer` 6-vs-8 — had been missed by every regex-based pass,
which is the argument for the gate rather than for another audit.

### phase-72 `DatagramSocket` shadowed a working implementation — RESOLVED

`net_phase_e::register_re7_datagram_socket` owns `java/net/DatagramSocket`. The
slot-based phase-72 set is deleted: it registered later and won every overlapping
key in synthetic-jdk builds (the broken set beating the side-table-based one), and
being `#[cfg(feature = "synthetic-jdk")]` it also meant the four keys it owned
ALONE (`isBound`, `getLocalAddress`, `setBroadcast`, `getBroadcast`) did not exist
at all in the default build. All four moved to RE7, which now covers the class in
both builds.

### The phase-72 fixes were LATENT — RESOLVED by moving them to the live path

`DatagramSocket.connect`/`disconnect`/`isConnected` are implemented on RE7,
against its side table and a real UDP fd (`FdTable::udp_disconnect`).
`HttpExchange.getLocalAddress`/`getRemoteAddress` moved into
`net_phase_e::register_re10_http_server` — the registrar that MINTS the exchange
and captures those addresses. Producer and consumer now live together, which is
the whole lesson: the producer half was real-JDK-live while the consumer half was
gated, so the getters resolved to the abstract declaration and threw
`AbstractMethodError`.

### `StackFrame.getDescriptor()` still threw — FIXED, and the cause was elsewhere

The exception never came from the expansion path. `StackWalker.StackFrame`
declares `getDescriptor()` as a DEFAULT method whose body is
`throw new UnsupportedOperationException()`; with no native on the concrete
carrier, `invokeinterface getDescriptor` landed on that default and threw on every
frame, including one from a walker built WITH `RETAIN_CLASS_REFERENCE`.
Registering it on the class intercepts the default.

The related `RETAIN_CLASS_REFERENCE` divergence had its own cause:
`java.lang.StackFrameInfo` is package-private in `java.base`, so when it cannot be
loaded CratonVM synthesizes a stub with ANONYMOUS fields — and
`set_field_by_name(sf, "flags", ..)` wrote nothing while
`get_field_by_name` read back null, so the check failed open on every frame. The
flag now has a slot that layout does have (`SF_FLAGS_FALLBACK`), used only when
the named write does not take.

Found while fixing it and fixed too: a duplicate `StackWalker.getInstance` pair in
`register_java_lang_extras_natives` that allocated the walker and wrote none of
its fields — no `options`, no `retainClassRef`. It registered after the real one,
so in `--synthetic-jdk` builds every walker came back with its options lost.

---

## What did NOT change, and why that is the finding

Two of the fifteen items are resolved as decisions rather than code:

* **JDBC in real-JDK mode** — registering the synthetic surface there would shadow
  the application's own driver. The `register_p68_xml`/Tomcat regression is the
  precedent, and the reasoning is now on the registrar.
* **JEP 486** — adopting it would disable the only sandbox this VM has.

Both are recorded next to the code they govern rather than in a tracking file,
which is the same move that let the census document be deleted: a decision with a
reason at the decision point does not need a document to remember it.

## Method note

Every behavioural claim above was measured three ways — HotSpot 25, CratonVM
before, CratonVM after — over three probe programs. The standing lesson from the
previous round applied again and earned its keep twice:

* `Document.getElementById` and `URLClassLoader.close` already matched HotSpot on
  the BEFORE arm for the paths the first probes exercised, so those probes could
  not have detected a regression OR a fix. Both were rewritten until they
  distinguished the arms (`close` needed a class the loader had not yet loaded);
* the first `Files.getOwner` fix measured as no change at all, and reading the
  stack trace — not the diff — showed the throw was coming from real JDK bytecode
  because the native was registered in a synthetic-only registrar.

Probe the specific site a change claims to fix, and read the failure, not just
the verdict.
