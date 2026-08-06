# `TomcatServletWebServerFactoryTests`: intermittent STW cross-thread JIT takeover hang — RESOLVED

**Status: FIXED 2026-07-27 (`fix/tomcat-stw-takeover-20260726`); the
2026-08-05 "REGRESSED" note is WITHDRAWN 2026-08-06.** The July root cause and
fix stand. The 08-05 sighting was one full-suite observation on a binary that
predates `383e7f5cf`, and it does not survive a controlled rerun: **72 runs**
across two binaries produced **zero** STW takeover stalls, with the census
instrumentation demonstrably live throughout (126 logs carry its `[stw-arrive]`
lines).

## Why the regression note was withdrawn

The note read a class going silent past the 300s shard ceiling as "the same
'everything froze mid-stream' signature as the original bug". Three things
undercut that, and none of them was available to that session:

**1. The binary it was observed on had a different, suite-wide silent-failure
mechanism.** `craton-fullsuite-azure-20260805-s8` ran `1078f6f05c`, which
predates `383e7f5cf` — *a recycled `JitInvokeInfo` address let one call site
serve another's dispatch*. On that binary a compiled call site can inherit the
previous site's native resolution and return its value, which killed classes
across the suite with no fixed face and, in the Flyway case, with no result line
at all (see `flywayautoconfigurationtests-timeout-jit-site-cache-aliasing-FIXED-20260805.md`
and the four Mockito/ByteBuddy pages retired against the same commit). "No
`SBRUNNER_RESULT`, output stops" was not a signature unique to STW on that
binary; it was the week's most common failure mode.

**2. The bug has a named, positive signature, and it never fired.** The July
investigation left the exact instrument for this:
`CRATONVM_DBG=stw-census,vm-state` makes the round-64 census print
`[stw-census] rounds=64 pending=1 taken=0 ...` and name the holdout native. It
was enabled for **every** run below. `STW cross-thread JIT takeover is still
waiting for cooperative mutators` appears **zero** times.

That silence is meaningful because the instrument was verified live rather than
assumed: the same logs carry 19–20 `[stw-arrive]` lines each, and one
non-completing run's *final* line is `[stw-arrive] tid=0 gen=6 arrived=4
expected=4` — the STW barrier completing, `arrived == expected`. That is the
exact inverse of the `pending=1 taken=0` this page documents.

**3. Every non-completing run in the rerun was the host, and provably so.**

## The rerun

Fixture `/data/data/springboot-jsonreader-deprecation-20260718`, one process per
class, the runner's own craton knobs (`CRATONVM_REAL=net-sockets,aqs`,
`CRATONVM_THREADS=-default-watchdog`, `CRATONVM_JIT=rootsnap-cache`),
`--Xmx 4g`, `CRATONVM_DBG=stw-census,vm-state`. "ssl-subset stress" is this
page's own harness: the class's `ssl*` methods repeated 4x in one JVM.

| Arm | `origin/dev` (`c9fc71c9a`) | the 08-05 suite binary (`1078f6f05c`) |
|---|---|---|
| ssl-subset stress | 24 runs — 22 completed, all 32/32 | 10 runs — 8 completed, all 32/32 |
| full class, sequential | 12 runs — 9 completed, all 132/132 | 8 runs — 5 completed, all 132/132 |
| full class, **6 JVMs at once x 3 rounds** — the shard's shape | 18 runs — **18/18 at 132/132** | — |
| **STW takeover stalls** | **0** | **0** |

The 12 runs that did not complete are all in one host-OOM band, dissected below;
every one of them was still writing output when its cap fired.

Both binaries' arms ran **concurrently** so they shared one load regime; on this
host a serial A/B is not a measurement.

### The non-completing runs are a host OOM episode, not hangs

Six runs (three per binary) hit the 900s cap. They are not distributed — they
fall in one wall-clock band, the *same* band on both binaries:

| | dev tip | 08-05 binary |
|---|---|---|
| before | runs 1–3 OK (00:46–00:56) | runs 1–2 OK (00:52–00:57) |
| **the band** | **4, 5, 6 killed** (01:10, 01:26, 01:41) | **3, 4, 5 killed** (01:11, 01:26, 01:42) |
| after | runs 7–8 OK (01:56, 01:59) | runs 6–8 OK (01:56–02:04) |

Those kill times are exactly 900s apart: every one burned its full cap. Outside
the band the same class completes in 4–5 minutes on both binaries.

What the band was: `dmesg` records system-wide OOM from 01:46:17 to 01:53:55 —
the kernel killed another session's `rustc` twice, plus `systemd` and
`(sd-pam)`. Load average reached **229**; memory sat at 30 of 31 GB used with
1 GB available. **No `cratonvm` process was OOM-killed** (0 hits in `dmesg`), so
these are starvation, not death.

And they were still running when killed, which is the point this page's own
symptom section turns on. Its signature is *"output simply stops — no further
log lines of any kind, not even the recurring `[moving-young]` GC warnings"*.
Here the final line at the kill instant is fresh output in every case: a Tomcat
connector line, a `cratonvm_gc` WARN, a `[stress] BEGIN` marker, or the
`[stw-arrive] ... arrived=4 expected=4` above. Slowed to a crawl by a box under
15x its core count, not frozen.

### The concurrent arm's first result was my own harness, and it is worth recording

The 6-JVM arm above is the second run of it. The **first** returned
`persistSession` failing in **9 of 18** runs:

```
AssertionFailedError: [Session error s1=null:… s2=…:… s3=null:…]
expected: "1785986620417" but was: "null"
  AbstractServletWebServerFactoryTests.persistSession:819
```

A reproducible 50% failure under concurrency looks exactly like a real
concurrency defect, and it is not one. Spring Boot's `ApplicationTemp` derives
the session store's path from `java.io.tmpdir`, so six concurrent copies of the
**same** class shared one store and clobbered each other's `SESSIONS.ser`. This
page's own reproduction recipe sets `TMPDIR` to a per-run scratch directory —
that line is load-bearing, and I had dropped it when rebuilding the harness.

Giving each process its own `TMPDIR` + `-Djava.io.tmpdir` took the same arm,
same binary, same host, from 9/18 failing to **18/18 at 132/132**. Sequential
arms never showed it, because they never overlap. If you fan this class out,
isolate the temp directory.

## What this page is still good for

The 2026-07-27 root cause, fix and diagnostic below are unchanged and remain the
reference for this bug class. In particular the **reusable lesson** stands: for
any `pending=N taken=0` STW stall, go straight to
`CRATONVM_DBG=stw-census,vm-state`; the census names the exact native the
holdout sits in (`state="native:<class>.<method><desc>"`) and marks it
`blocked=false` against a sea of `blocked=true` peers.

The 08-05 note also adds a trap worth keeping: **"no result line" is not a
signature.** Before reading silence as a specific bug, run the instrument that
would name that bug, and check it is live — and check what else the binary in
question was known to do silently.

---

# Original record (2026-07-27), left as written

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
(`keycloak-model-stw-takeover-hang-eventloopgroup-shutdown-FIXED.md`,
`wildfly-standalone-boot-stw-jit-takeover-hang-FIXED.md`, the
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

## Verification (2026-07-27)

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
(96 executions of the 16 `ssl*` methods).

## Residuals also fixed in 2026-07-27 (this class went to 132/132)

`origin/dev` failed this class 3/132 every run, plus one intermittent. All four
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

## Reproduction harness

```
cd module/spring-boot-tomcat
export CRATONVM_REAL=net-sockets,aqs CRATONVM_THREADS=-default-watchdog CRATONVM_JIT=rootsnap-cache
export CRATONVM_DBG=stw-census,vm-state   # names the holdout thread
export TMPDIR=/data/tmp/<scratch>
<cratonvm-exe> --java-home <jdk25> --Xmx 2g \
  -Dfile.encoding=UTF-8 -Djava.awt.headless=true -Djava.io.tmpdir=/data/tmp/<scratch> \
  -cp <module classpath> SbRunner org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactoryTests
```

Restricting the selection to the `ssl*` methods and repeating them several
times in one JVM raised the pre-fix hit rate to roughly 1 in 3-4 without
lengthening a run.

## Affected classes

| Module | Class | Outcome |
|---|---|---|
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactoryTests` | **132/132 PASS**; no STW stall in 72 further runs on 2026-08-06 |
