# `TomcatServletWebServerFactoryTests`: intermittent STW cross-thread JIT takeover hang

**Status: OPEN — REGRESSED 2026-08-05.** Previously root-caused and fixed
2026-07-27, on branch `fix/tomcat-stw-takeover-20260726`. Filed 2026-07-26
while verifying
[`tomcatservletwebserverfactorytests-ssl-clientauth-peercert-residuals-FIXED.md`](../../internal/fixed-suite-bugs/springboot/tomcatservletwebserverfactorytests-ssl-clientauth-peercert-residuals-FIXED.md).

## Regression note (2026-08-05)

Full-suite rerun `craton-fullsuite-azure-20260805-s8` (all-jit) hit the
300s timeout ceiling on this class again:
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-azure-20260805-s8/all-jit/logs/module_spring-boot-tomcat.org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactoryTests.{out,err}.log`.

The `.out.log` shows steady per-test progress (repeated
`Tomcat initialized with port 0 (http)` / connector start/stop cycles,
consistent with `132` short-lived embedded-Tomcat lifecycles) reaching at
least connector instance `http-nio-auto-112` before output simply stops —
no `SBRUNNER_RESULT` line, no JUnit summary, no further log lines of any
kind (not even the recurring `[moving-young]` GC warnings that appear
throughout the rest of the run). That is the same "everything froze
mid-stream, not just slowed down" signature as the original bug, not a
slow-but-progressing run. HotSpot passes this class in 75.7s (132/132,
`hotspot-baseline-latest.tsv` row 136); a genuine host-load slowdown would
still be producing periodic log output, not going silent.

`ps aux` on the host at investigation time showed no lingering/orphaned
`cratonvm` process at this run's exact binary path — ruling out the
"runner's timeout-kill failed to reap it" confound — but did show several
*other* concurrent `cratonvm`/`cargo build` processes from unrelated
sessions, so this is plausibly (not confirmed) a genuine STW deadlock,
recurring either because a *different* blocking native (not one of the
four SSL-stream natives + accept paths the 2026-07-27 fix bracketed) is
now missing its `begin_blocking_region()`/`end_blocking_region()` bracket,
or because the fix regressed. Not re-root-caused this session — the
2026-07-27 fix's own diagnostic recipe
(`CRATONVM_DBG_STW_CENSUS=1 CRATONVM_DBG_VM_STATE=1` against a live hung
process, naming the exact `pending=1,blocked=false` native via
`state="native:<class>.<method><desc>"`) is the fastest way to confirm and
localize this on the next pass.

## Original fix (2026-07-27), left as written below

The same session closed every other CratonVM-specific failure in this test
class, taking it from **129/132 with an intermittent hang** to **132/132**.

## Symptom

A full run of `TomcatServletWebServerFactoryTests` (132 test methods, most of
which start and stop a fresh embedded Tomcat) occasionally hung indefinitely
partway through, always right after a fresh HTTPS connector for one of the
`ssl*` test methods started accepting. The only log output was the recurring
VM warning:

```
WARN cratonvm_vm::runtime::interpreter: STW cross-thread JIT takeover is still waiting for cooperative mutators rounds=64 pending=1 taken=0
```

`pending=1 taken=0` repeated forever: exactly one thread never reached the
cooperative STW acknowledgement point, and no in-JIT peer could be forcibly
taken over instead.

## Root cause

`javax/net/ssl/SSLSocketInputStream.read()` / `read([BII)` and
`javax/net/ssl/SSLSocketOutputStream.write(I)` / `write([BII)` in
`native-builtins/src/phases_late/ssl_security.rs` called
`servlet::s2_tls_read` / `s2_tls_write` — genuine OS-level blocking socket
I/O — **without** bracketing them in
`NativeContext::begin_blocking_region()` / `end_blocking_region()`.

A thread parked inside that read is therefore still counted as a cooperative
mutator by the STW protocol, but it cannot execute bytecode and so can never
reach a safepoint poll. Worse, the bytes it waits for are produced by a peer
thread **in the same process** — Tomcat's `NioEndpoint$SocketProcessor`
serving the HTTPS request — which *does* stop at the barrier. Genuine mutual
deadlock: STW waits on the client thread, the client thread waits on the
server thread, the server thread waits for STW to finish.

Same bug shape as several prior fixes here
(`../keycloak/keycloak-model-stw-takeover-hang-eventloopgroup-shutdown-FIXED.md`,
`../wildfly/wildfly-standalone-boot-stw-jit-takeover-hang-FIXED.md`, the
`net_phase_e` `HttpClient` fix): a blocking native call missing the
blocking-region bracket its sibling call sites already had.

### The diagnostic that found it

`--stack-dump-on-timeout` is useless for this bug class by construction: it
reported `0 thread(s) dumped; no Java threads responded — main thread is in
native (Rust) code`, because dumping needs threads to reach a safepoint, which
is exactly what cannot happen. Two other signals did the job:

1. `/proc/<pid>/task/*/comm` + `wchan` on the live hung process: every thread
   sat in `futex_do_wait` except `main-vm`, which was in `wait_woken` — a
   socket wait.
2. `CRATONVM_DBG_STW_CENSUS=1 CRATONVM_DBG_VM_STATE=1`, which made the
   already-existing round-64 census name the holdout:

```
[stw-census] rounds=64 pending=1 taken=0 blocked=24 alive=26
  t0 os_tid=4135765 name="main" blocked=false ready=true snapshot=285
     state="native:javax/net/ssl/SSLSocketInputStream.read([BII)I"
     top=org/apache/hc/client5/http/ssl/AbstractClientTlsStrategy.executeHandshake@150
```

Every other thread had `blocked=true`; `main` — inside the unbracketed
native — was the single `pending` holdout.

**Reusable lesson:** for any `pending=N taken=0` STW stall, go straight to
`CRATONVM_DBG_STW_CENSUS=1 CRATONVM_DBG_VM_STATE=1`. The census names the
exact native the holdout is stuck in (`state="native:<class>.<method><desc>"`)
and marks it `blocked=false` against a sea of `blocked=true` peers.

## Fix

`native-builtins/src/phases_late/ssl_security.rs` — bracket all four
`SSLSocket{Input,Output}Stream` I/O natives. The array target of `read([BII)`
is pinned across the block (it is written after the thread resumes, so a
moving collection during the park would otherwise leave a stale `ObjectRef`),
and `write([BII)`'s drain loop carries its error out instead of returning
from inside the region — an early `return` between begin/end would leave the
thread permanently marked blocked, the mirror-image bug (a live mutator the
barrier stops waiting for).

### Hardening applied at the same time (same family, not the trigger here)

- `native-builtins/src/t27_tls.rs`: `rustls_stream_read`/`_write`/`_close` and
  `rustls_client_peer_cert_chain_der` held the **process-wide** `sreg()`
  registry mutex across blocking socket I/O. A thread parked on a plain mutex
  inside a native is invisible to the safepoint protocol in exactly the same
  way, so this can reproduce the identical `pending=1` stall with two
  concurrent TLS sockets. Each stream now owns an `Arc<Mutex<..>>`; the
  registry lock is held only long enough to clone that handle. This is the
  fix already applied to the ACCEPT path (`h2-testnetutils-accept-close-deadlock`)
  whose sibling read/write paths were missed at the time.
- `native-builtins/src/servlet.rs`: identical treatment for `s2_tls_read`/
  `_write`/`_close` and the `s2_registry()` lock. `TlsEntry` also gains a
  `try_clone`d raw `TcpStream` so `Socket.shutdownInput/Output` and
  `set/getSoTimeout` never wait on the per-stream mutex — `shutdownInput` is
  how a caller unblocks a parked reader, so it must not be able to block on it.
- `native-builtins/src/servlet.rs`, `native-builtins/src/phases_early.rs`:
  both callers of `s2_blocking_accept` (`ServerSocketChannel.accept`,
  `ServerSocket.accept`) now bracket the unbounded blocking accept.

## Verification

`module/spring-boot-tomcat`, `TomcatServletWebServerFactoryTests`, JDK 25,
Azure Linux host. "ssl-subset stress" = the 16 `ssl*` methods repeated 4× in
one JVM, which concentrates HTTPS connector churn without lengthening a run.

| Binary | Harness | Runs | STW hangs |
|---|---|---|---|
| pre-fix (`origin/dev`) | ssl-subset stress | 7 | **2** |
| pre-fix (`origin/dev`) | full class | 10 | 0 |
| post-fix | ssl-subset stress | 24 | **0** |
| post-fix | full class | 32 | **0** |

The hang concentrates in the ssl-heavy harness (~1 in 3.5 there); the
full-class rate is low enough that 10 pre-fix runs happened not to hit it,
consistent with the "1 in 3-5" the original report estimated from a smaller
sample. Zero recurrences post-fix across 56 runs.

Test outcomes on the final binary: **132/132 PASS on every one of 20
consecutive full-class runs**, and 0 failures across 24 ssl-subset stress runs
(96 executions of the 16 `ssl*` methods). `origin/dev` fails the same class
3/132 on every run plus one intermittent — see below.

`cargo test -p cratonvm-native-builtins -p cratonvm-native-io --lib`:
3098 passed / 5 failed — the identical 5 failures a pristine `origin/dev`
build produces (`cglib_enhancer`, `lang_string`, `logmanager` ×2,
`regex_matcher`), i.e. no regression.

## Residuals also fixed (this class is now 132/132)

`origin/dev` fails this class 3/132 every run, plus one intermittent. All four
were real VM bugs, not environment artifacts.

### 1. `persistSession`, `getValidSessionStoreWhenSessionStoreNotSet`

`IllegalStateException: Existing directory '…' does not have the permissions
[OWNER_READ, OWNER_WRITE, OWNER_EXECUTE]`.

`PosixFilePermissions.asFileAttribute` was a native returning **null** — and
it shadows the real JDK's anonymous-class implementation even in real-JDK
mode — while `Files.createDirectory`/`createDirectories`/`createTempDirectory`/
`createTempFile` ignored their `FileAttribute[]` argument outright. Every such
directory was created with the process umask (0755) instead of the requested
0700, so Spring Boot's `ApplicationTemp` rejected its own temp directory on
the second call. Fixed in `native-builtins/src/phases_late/nio_file.rs`;
`createTemp*` now also default to the JDK's 0700/0600 as `TempFileHelper`
does.

Behind that sat a second gap: `sun.nio.fs.UnixNativeDispatcher.getpwuid` had
no native at all, so `UnixFileAttributes.owner()` — evaluated as an argument
to Spring's ownership assertion — threw `UnsatisfiedLinkError`. Added
`getpwuid`/`getgrgid` (passwd/group lookup with the decimal-id fallback
`UnixUserPrincipals` itself uses) in `native-io/src/lib.rs`, and
`Files.readAttributes` now populates the real `st_uid`/`st_gid` instead of
reporting every file as owned by uid 0.

A standalone probe confirms CratonVM now matches HotSpot exactly:
`createDirectory` 0700, `createTempDirectory` 0700, `createTempFile` 0600.

### 2. `sslWithHttp11Nio2Protocol`

`SSLHandshakeException: handshake read: Resource temporarily unavailable` —
deterministic, open since before the client-auth work. **No NIO2 server could
serve a byte**, for four stacked reasons in `native-io/src/async_socket.rs`,
each hidden behind the previous:

- Handler-form ACCEPT/CONNECT/WRITE completions were parked in a queue drained
  *only* opportunistically by the next AIO native call on a VM thread, and the
  dedicated dispatcher thread was started only by the read/write natives. An
  idiomatic NIO2 server arms one `accept()` and then waits — so its
  `CompletionHandler` was never invoked at all.
- Read/write on an **accepted** channel rejected it outright (`IOException:
  read: not connected`): those natives only understood channels backed by an
  fd_table fd (the Future-form `connect` client path), not the
  `aio_registry`-backed channels `accept` produces.
- The **timed** overloads `read`/`write(ByteBuffer, long, TimeUnit, A,
  CompletionHandler)` were not registered at all, and Tomcat's
  `SecureNio2Channel` uses only those. (The timeout is now honoured via
  `SO_RCVTIMEO`/`SO_SNDTIMEO` on the worker's private handle.)
- `AsynchronousSocketChannel.setOption` was not registered, so
  `Nio2Endpoint.setSocketOptions` hit `AbstractMethodError`, logged "Error
  setting socket options", and dropped every accepted connection.
  `getOption`/`supportedOptions`/`getRemoteAddress` were missing too — the
  same `NetworkChannel` abstract-method trap already documented for the
  blocking channels in `socket_channel.rs::supported_socket_options`.

A 40-line standalone `AsynchronousServerSocketChannel` echo probe reproduced
the whole chain (prints `ACCEPTED` … `PONG` on HotSpot, nothing on CratonVM)
and now matches HotSpot exactly. **Any NIO2 server workload was affected, not
just this test.**

### 3. `shouldUpdateSslWhenReloadingSslBundles` (intermittent)

`SSLHandshakeException: layered socket handshake state missing`, ~1 run in 6
on `origin/dev`. `ensure_layered_handshake_started` held the `SSLSocket`'s
`ObjectRef` across its `begin_blocking_region()`/`end_blocking_region()`
unpinned. A moving young collection during the handshake relocates the object,
so every post-handshake write (`sock_set_for_create`, `NEW13_SOCK_TLSID`, the
`SSLSession`) landed on a stale reference; the socket kept its *pending* tls
id, and the next resolution drove the handshake a second time and found the
pending entry already consumed. Fixed by pinning across the region — precisely
the hazard `new13_do_create_socket`'s own blocking-region comment describes,
at the sibling call site that never got the same treatment.

## Reproduction (pre-fix)

```
cd module/spring-boot-tomcat
export CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_ROOTSNAP_CACHE=1
export CRATONVM_DBG_STW_CENSUS=1 CRATONVM_DBG_VM_STATE=1   # names the holdout thread
export TMPDIR=/data/tmp/<scratch>
<cratonvm-exe> --java-home <jdk25> --Xmx 2g \
  -Dfile.encoding=UTF-8 -Djava.awt.headless=true -Djava.io.tmpdir=/data/tmp/<scratch> \
  -cp <module classpath> SbRunner org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactoryTests
```

Restricting the selection to the `ssl*` methods and repeating it several times
in one JVM raises the hit rate to roughly 1 in 3-4 without lengthening a run.

## Affected classes

| Module | Class | Outcome |
|---|---|---|
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactoryTests` | **132/132 PASS**; no STW hang in 32 post-fix runs |
