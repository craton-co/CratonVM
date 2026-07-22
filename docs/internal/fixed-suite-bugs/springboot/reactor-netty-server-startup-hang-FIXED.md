# spring-boot-reactor-netty: `NettyReactiveWebServerFactoryTests` HANG — FIXED

**Status: FIXED — verified 2026-07-20.** The hang is gone. Fixing it exposed a
residual set of 12 test failures; 9 are now fixed as well (see below). Two
narrower, unrelated bugs remain open — filed as their own docs.

## Original symptom (recap)

`NettyReactiveWebServerFactoryTests` timed out (HANG) with zero JUnit output,
settling into a constant-rate repeating `gen_heap::get_field: out-of-bounds
field read dropped` guard warning against
`org/junit/jupiter/engine/execution/InterceptingExecutableInvoker`. Filed
2026-07-17 as OPEN/UNCONFIRMED — see the original doc's git history for the
full original writeup (log-analysis-only, no root cause identified that
session).

## Root cause of the hang — already fixed by unrelated prior work

Reproducing on current `dev` (`06f832843`) with the suite runner's real
environment (`CRATONVM_REAL_NET_SOCKETS=1`/`CRATONVM_REAL_AQS=1`/
`CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`/`CRATONVM_ROOTSNAP_CACHE=1`, matching
`run-spring-boot-suite.ps1`) shows the class no longer hangs at all — it
completes in ~25-55s depending on which of the fixes below are present. The
guard-warning signature described in the original doc (repeating
`InterceptingExecutableInvoker` out-of-bounds field read, zero JUnit output)
is *exactly* the livelock signature independently root-caused and fixed
2026-07-18 in
[`junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md`](junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md)
(a foreign-function downcall adapter fast path in `try_stackless_invoke` that
was selected by method name alone — `invoke`/`invokeExact`/`invokeBasic` —
without first proving the receiver was actually a `MethodHandle`; JUnit's
zero-field `InterceptingExecutableInvoker.invoke` matched the name and
entered that unrelated path, generating an unbounded out-of-bounds probe
loop). That fix, already on `dev`, resolves this class's hang as a side
effect — no additional hang-specific fix was needed this session.

## Residual: the class was never actually run to completion, so its real
## FAIL/HANG signature was previously invisible

With the hang gone, the class runs its full 36 tests and originally showed
**12 failures**. Investigated and fixed 9 of them via three distinct,
unrelated CratonVM bugs (all on branch `fix/reactor-netty-hang-20260720`,
worktree `CratonVM-reactor-netty-hang-20260720`):

### Fix 1 — `FileSystemProvider.getPath(jar: URI)` lost the jar/entry split

**Symptom:** 9 SSL tests (`basicSslFromFileSystem`, `basicSslFromClassPath`,
`sslWithPemCertificates`'s resource half, all `sslNeeds/WantsClientAuth*`
variants) failed with `IllegalArgumentException: Package
'org.springframework.boot.web.server.reactive' did not contain resources:
[test.jks]` from Spring's `@WithPackageResources`/`Resources.addPackage`
test-support extension.

**Root cause:** `Resources.addPackage` calls
`ClassLoader.getResources(pkg)` (correctly found the jar URLs — not the
bug), then `Paths.get(sourceUri)` on each. For a `jar:` URI, real
`Paths.get(URI)` dispatches (per scheme) to
`FileSystemProvider.getPath(URI)` on the matching installed provider — but
CratonVM's native for that method (`native-builtins/src/phases_late.rs`)
only understood `file:` URIs (reading URI field slots that are never
populated for non-`file` schemes), producing a garbage single-segment path
(literally the string `"jar"`, the bare scheme). `Path.of(URI)` had the
correct jar-aware logic (`p57_jar_uri_to_entry_path` + mounting a
jar-backed `FileSystem`) but `FileSystemProvider.getPath` — the method
`Paths.get(URI)` actually calls at runtime for non-`file` schemes — never
used it. A second, contributing gap: the URI text scan
(`p57_uri_full_text`) only checked synthetic-URI field slots `0..=5`, but
`URL.toURI()`'s 7-field layout puts the raw full-URI text at slot 6.

**Fix:** `FileSystemProvider.getPath` now runs the same jar-aware check
`Path.of(URI)` already had, and `p57_uri_full_text` scans slot 6 too.

### Fix 2 — bind() `AddrInUse`/etc. never became a real `java.net.BindException`

**Symptom:** `portInUseExceptionIsThrownWhenPortIsAlreadyInUse` asserted
`PortInUseException` but got a generic `WebServerException: Unable to start
Netty`.

**Root cause:** Three native bind paths
(`native-io/src/{net,socket_channel}.rs`, `native-builtins/src/net_phase_e.rs`)
mapped `AddrInUse`/`AddrNotAvailable`/`PermissionDenied` to a plain
`IOException` whose *message* merely started with the text `"BindException:
..."` — not a concrete `java.net.BindException` object. Spring Boot's
`PortInUseException.throwIfPortBindingException` walks the cause chain with
`instanceof BindException`, which a bare `IOException` never satisfies, so
it always fell through to the generic wrapper. (The exact same
text-prefix-instead-of-real-type gap this file's own comments already flag
as fixed for `ConnectException`/`SocketTimeoutException` — just not yet for
bind failures.)

**Fix:** added a typed `RuntimeError::BindException` variant (mapped to
`java/net/BindException`, mirroring the existing `ConnectException`
pattern) and switched all three call sites to it.

### Fix 3 — a refused non-blocking connect lied that it had connected

**Symptom:** `whenServerIsShuttingDownGracefullyThenNewConnectionsCannotBeMade`
timed out after 30s waiting for a `ConnectException` that never arrived.
Isolated repro showed the *real* shape: a non-blocking connect to a
just-closed loopback listener got `reactor.netty.http.client.
PrematureCloseException: Connection prematurely closed BEFORE response`
caused by `ConnectException: write: Connection refused` — i.e. the failure
surfaced on the *first write*, not at connect time.

**Root cause:** `native-io/src/socket_channel.rs`'s non-blocking connect
path has a Windows-specific fast probe: a loopback connect that's
synchronously known to be refused (`nb_connect::StartConnect::
DeferredFailure`) is kept as a live, pollable socket so the JDK selector
can deliver `OP_CONNECT` and let `finishConnect()` report the error
asynchronously (correct, and exactly what the selector's own
`probe_connect_status` already does for this state — it returns `Ready`
immediately for a `ConnectFailed` registry entry). But the *caller*,
`sc_connect_inner`/`sc_connect_bound`, additionally set `F_CONNECTED=1` and
returned `true` from `connect()` itself — which is the documented JDK
contract for "connected synchronously, no `finishConnect()` needed." Netty's
`AbstractNioChannel.AbstractNioUnsafe.connect()` treats a `true` return as
an unconditional, immediate success and fulfills the connect promise right
there, without ever registering `OP_CONNECT` interest or calling
`finishConnect()` — so the saved failure was never observed until the
channel's first write hit the (never actually connected) raw socket,
surfacing as a generic "Connection refused" instead of a
`ConnectException`/`BindException` at connect time. This is very likely the
same root mechanism behind much of the incidental
`PrematureCloseException`/"Connection refused"-during-write noise visible
throughout unrelated Spring Boot Netty test logs — not confirmed against
every occurrence, but the shape matches.

**Fix:** `connect()` now returns `false` (not `true`) for the
`DeferredFailure` case, leaving `F_CONNECTED` unset. `probe_connect_status`
already reports `Ready` for a `ConnectFailed` entry, so the selector
delivers `OP_CONNECT` normally and the reactor's own `finishConnect()` call
reports the typed failure. Also fixed `sc_is_connection_pending()`, which
only recognized `Connecting` (not `ConnectFailed`) as "pending" — a reactor
that checks `isConnectionPending()` before `finishConnect()` (a documented,
real pattern — see this file's own comment on
`InternalConnectChannel.onIOEvent`) needs `true` for both. Removed a
provably-dead duplicate `DeferredFailure` match arm in `sc_connect_bound`
along the way (flagged by the compiler as unreachable; verified unreachable
by hand — the preceding unconditional arm already covers every case it
would have matched).

Verified via `cargo test -p cratonvm-native-io --lib` (354/354 pass,
including the existing `t19_5_bind0_port_already_in_use_throws_bind_exception`
and `closed_loopback_port_reports_connection_refused` tests) and a standalone
non-blocking-connect repro comparing plain `java.net.Socket` (worked before
and after) against `reactor.netty.http.client.HttpClient` (failed before with
exactly the `PrematureCloseException`/write-refused shape, succeeds after).

### Fix 4 (partial) — `SunX509KeyManagerImpl`-compatible alias ordering

While investigating `sslWithPemCertificates` (still OPEN — see below), found
and fixed a real, independently-verifiable bug: `x509_manager.rs`'s
`build_key_manager_state` populated `client_aliases_by_key_type`/
`server_aliases_by_key_type` in keystore/file order, but real HotSpot's
default `SunX509`-algorithm `KeyManager` (`sun.security.ssl.
SunX509KeyManagerImpl`, what `KeyManagerFactory.getDefaultAlgorithm()`
actually names) builds a plain `HashMap<String,X509Credentials>
credentialsMap` from the keystore and derives `getClientAliases()`/
`chooseClientAlias()`'s candidate order from *that* map's bucket-iteration
order — not file order. Verified against `NettyReactiveWebServerFactoryTests`'
own `test.p12` fixture (two ambiguous client identities, "spring-boot" and
"test-alias", same key type/validity, no EKU to disambiguate): real
HotSpot's `getClientAliases("RSA", null)` returns `[test-alias,
spring-boot]` (confirmed live, JDK 25), the *opposite* of file order
(`spring-boot` is physically first in the PKCS12 file — confirmed via
`openssl pkcs12 -info`). Traced this to Java's `String.hashCode()` +
`HashMap` bucket math for these two specific alias strings (a stable,
unsalted, frozen algorithm) — not any semantic preference.

**Fix:** added `java_hashmap_iteration_order` (replicating
`String.hashCode()` + `HashMap`'s bucket-spread function + capacity-growth
schedule) and reorder the by-key-type alias lists through it before
`chooseClientAlias`/`chooseServerAlias` consult them. Verified live:
`getClientAliases`/`chooseClientAlias` under CratonVM now return the exact
same `[test-alias, spring-boot]` order as real HotSpot for this keystore
(was `[spring-boot, test-alias]` before the fix). `cargo test -p
cratonvm-native-builtins --lib x509_manager`: 48/48 pass.

This fix is real and correct (bug-compatible with observable HotSpot
behavior) but did **not**, by itself, resolve `sslWithPemCertificates` —
see the residual doc for why.

## Verification

`NettyReactiveWebServerFactoryTests` via `run-spring-boot-suite.ps1`
(`-Exe` pointing at a release build with all fixes above, real suite-runner
environment): **34/36 tests pass** (was: HANG, 0/36 ever ran). The two
remaining failures at the time were unrelated, deeper bugs — filed
separately:

- [`sslWithPemCertificates` `rustls DecryptError`](pemcertificates-clientauth-rustls-decrypterror-FIXED.md) — **FIXED 2026-07-20**, now 35/36
- [`whenHttp2IsEnabledAndSslIsDisabledThenH2cCanBeUsed` HPACK decode failure](../../known-issues/springboot/h2c-priorknowledge-hpack-headerblock-decode-failure.md) — still OPEN

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-reactor-netty` | `org.springframework.boot.reactor.netty.NettyReactiveWebServerFactoryTests` |
