# Five FAILs from the 2026-08-07 full-suite sweep: `TestPgServer`, `TestTools`, `TestMemoryUnmapper`, `TestFileLock`, `TestTimer` — RESOLVED

## Status
**✅ RESOLVED AGAIN 2026-08-16 — item 2's reopened residual is closed, with a
root cause nobody had looked for.** Retired back to `docs/internal/`.

The 2026-08-16 reopening (preserved verbatim below) was right that the symptom
was live and wrong about where it lived. It read `TLS handshake failed: the
handshake process was interrupted` as a client-side *message-mapping* gap and
asked for the client's `SSLHandshakeException` to be given the real
certificate reason. The client was already capable of that — measured 57 ms and
the real OpenSSL certificate-verify error, on the same binary, whenever it was
the FIRST connection to the server. The reopening's own two hypotheses (the
message text, and the `PENDING_CONNECT_SOCK_ID_BASE` trigger points) are both
ruled out below.

What was actually broken is on the **server** side, and it is a liveness bug,
not a reporting one: `SSLServerSocket.accept()` ran the TLS handshake inline
and threw its failure out of `accept()`. `TcpServer.isRunning()` opens a
loopback socket and closes it again without any I/O, so H2 killed its own SSL
server's accept loop on the first liveness probe. The real JDBC client that
followed found a listening socket nobody was accepting from, waited out its
30 s read timeout, and reported the `WouldBlock` text. Fixed by deferring the
failure to the accepted socket, as JSSE does — see "Closed 2026-08-16" at the
end of this page.

```
                        before            after           HotSpot 25.0.3
TestTools verdict       FAIL              FAIL            FAIL   (all three at TestTools.java:656)
  reported as           SSLHandshake-     SSLHandshake-   SSLHandshake-
                        Exception: "the   Exception:      Exception: PKIX path
                        handshake         "certificate    building failed …
                        process was       verify failed
                        interrupted"      … (EE certificate
                                          key too weak)"
  wall clock            ~38 s             4.9 s           3.0 s
```

Items 1, 3, 4, 5 are unaffected — independently verified against HotSpot at the
time, and nothing found in either the reopening or today's work contradicts
them.

Original status, preserved: **✅ RESOLVED 2026-08-07.** Three were genuine
CratonVM defects and are fixed; two are H2-test-versus-JDK-25 incompatibilities
that real HotSpot 25.0.3 fails in exactly the same place, so they were never VM
defects. Every verdict below was decided by running the class on stock HotSpot
with the identical classpath before touching the VM.

| # | Class | Verdict | Now |
|---|-------|---------|-----|
| 1 | `TestPgServer` | CratonVM defect — `ReferenceQueue.poll()` returned one `enqueue()` twice | **PASS** (was NPE inside pgjdbc) |
| 2 | `TestTools` | CratonVM defect — `SSLSocket.connect()` ran the TLS handshake | reported symptom gone; class now fails where **HotSpot fails too** |
| 3 | `TestMemoryUnmapper` | NOT a VM defect — HotSpot 25 fails identically | unchanged, correctly |
| 4 | `TestFileLock` | CratonVM defect — `Files.createFile` was not `CREATE_NEW` | **PASS** |
| 5 | `TestTimer` | NOT a VM defect — HotSpot 25 fails identically | unchanged, correctly |

Measured on the Azure Linux host, `/home/victor/jdk25` (Temurin 25.0.3+9),
`--java-home /home/victor/jdk25 --nojit --Xmx 1g`, one class per scratch CWD.

---

## 1. `TestPgServer.testDateTime` — `ReferenceQueue.poll()` returned the same reference twice

**Root cause.** `native_rq_poll` popped the queue with a literal
`head = ref.next`. The real JDK marks the LAST element of a `ReferenceQueue`
by SELF-LINKING it — `ReferenceQueue.enqueue` does
`r.next = (head == null) ? r : head`, and `reallyPoll` undoes that with
`head = (r.next == r) ? null : r.next`. `native_ref_enqueue` delegates to that
real bytecode whenever the receiver has the real `Reference` layout, so the
native `poll` was popping a list linked by a convention it did not implement:
after one `enqueue()`, `head` was left pointing back at the reference just
returned, and the next `poll()` handed out the same object again.

Isolated as a 12-line probe (`EnqProbe.java`), CratonVM vs HotSpot:

```
1c poll()==ra         = true   (expect true)
1d poll() again       = java.lang.ref.PhantomReference@62   (expect null)   <-- CratonVM
4b drained            = 2      (expect 1)                                   <-- CratonVM
```

**Why pgjdbc dies of it.** `SimpleQuery.unprepare()` / `setCleanupRef()` call
`clear()` + `enqueue()` on the query's own `PhantomReference`, and
`QueryExecutorImpl.processDeadParsedQueries` loops
`parsedQueryMap.remove(polled)` with **no null check** (the `castNonNull` there
is a no-op at runtime). The duplicate poll removed an entry that was already
gone, so `sendCloseStatement(null)` raised
`NullPointerException: ... because "statementName" is null` from inside the
driver — the exact trace the sweep reported.

**Fix.** `native-builtins/src/reference.rs`, `native_rq_poll`: treat
`next == ref_obj` as end-of-list. The GC auto-enqueue path uses the synthetic
convention (`next` = old head, or null on an empty queue) and never
self-links, so it is unaffected either way.

**Verified.** `EnqProbe` matches HotSpot line for line. `TestPgServer`
RC=0 (687 s, then 809 s on a re-run); it had failed at 508–656 s.

## 2. `TestTools.testSSL → runServer` — `SSLSocket.connect()` was running the TLS handshake

Two independent problems sat on top of each other here.

### 2a. A `dev` regression made the class die 8 minutes before the reported symptom

On today's `origin/dev` (`1082eb446`) `TestTools` died in **under 1 second** at
`testConsole` with `UnsatisfiedLinkError: no awt in java.library.path`, never
reaching the failure this report is about. The sweep's own binary
(built 02:33 from an earlier `dev`) got past it. Cause: `load_library_or_throw`
began actually raising the `UnsatisfiedLinkError` it had previously computed
and dropped, and `awt` was never on `is_vm_provided_jdk_library`'s allowlist —
so merely CONSTRUCTING a `java.awt.event.ActionEvent` (which initialises
`java.awt.Toolkit`) now threw, on a VM that otherwise models AWT well enough to
answer `HeadlessException` for `new java.awt.Button()` exactly as headless
HotSpot does.

MEASURED, JDK 25.0.3 Linux x64, one cold `System.loadLibrary` per name:

```
LOADS : awt awt_xawt fontmanager javajpeg lcms jsound splashscreen freetype
        mlib_image jsig zip net nio management instrument
THROWS: sunec, jvm, harfbuzz, a made-up name
```

Every LOADS name has a `<java.home>/lib/lib<name>.so`; no THROWS name does
(`libjvm.so` lives one level down in `lib/server`, off `java.library.path`).
HotSpot's rule is FILE PRESENCE, not an allowlist — so a hardcoded list can
never keep up with it.

**Fix.** `native-builtins/src/lang_system.rs`: added `jdk_image_ships_library`,
a presence test against the JDK image directory (`<java.home>/lib` on Unix,
`<java.home>/bin` on Windows), consulted after the existing allowlist and only
for a BARE name. `zip` stays excluded via `DYNAMIC_ALREADY_LOADED`: HotSpot
refuses it for a dynamic reason (java.base has boot-loaded it by the time
`RJdkJni` asks), which file presence cannot model and which that corpus vector
pins.

### 2b. The reported symptom: `connect()` is not supposed to handshake

With 2a fixed the class reached the sweep's failure and reproduced it exactly
(`Expected: 0 actual: 1` at `TestTools.java:708`, 550 s).

`TestTools.java:708` is `assertEquals(exitCode, result)` inside `runServer`,
and `result == 1` means `Server.runTool` threw. A `--stack-sample-ms 3000`
capture put the main thread in
`Server.start → Server.isRunning → TcpServer.isRunning`, i.e. inside
`NetUtils.createLoopbackSocket(port, ssl)`.

`TcpServer.isRunning(boolean)` opens a loopback socket to its own server socket
and closes it again **without any I/O**. On HotSpot that is a TCP connect and
nothing more — JSSE runs the TLS handshake at the first read/write, at
`startHandshake()`, or at `getSession()`. CratonVM ran the whole handshake
inside `SSLSocket.connect()` (`new13_ssl_socket_connect` called
`new13_connect_and_handshake` directly), so `isRunning` inherited every
handshake failure *and* its timeouts. Measured with `TlsProbe.java`, the same
shape as `isRunning`:

```
CRATONVM (before): client FAILED after 60818ms:
  java.io.IOException: TLS handshake failed: the handshake process was interrupted
    at org.h2.security.CipherFactory.createSocket(CipherFactory.java:95)   <-- secureSocket.connect(...)
```

`Server.start()` therefore saw `isRunning` false 64 times and threw
`EXCEPTION_OPENING_PORT_2`, which `runServer` counted as exit 1.

**Fix.** `SSLSocket.connect(SocketAddress[, int])` now does the TCP connect
(so an unreachable peer still fails *there*, as JSSE does) and parks the
endpoint under a new id range, `servlet::PENDING_CONNECT_SOCK_ID_BASE`. The
handshake is the ORIGINAL `new13_connect_and_handshake`, run unchanged and
simply later, driven from the four JSSE trigger points the layered
(`createSocket(Socket wrapped, …)`) range already used —
`getSession`, `startHandshake`, `getInputStream`, `getOutputStream`. `close()`
releases parked state; `setEnabledCipherSuites`/`setEnabledProtocols` accept
and discard for the new range instead of trying to tear down and redo a
connection that has not been made yet.

**Verified.** The `runServer` call now returns
`exit=0 out=TCP server running at ssl://…:9001 (others can connect)`, byte-for-byte
what HotSpot prints. `TestTools` runtime fell from 550 s to 38 s.

### What `TestTools` still does, and why that is correct

The class still FAILs — **and so does stock HotSpot 25**, at the same line:

```
HOTSPOT : TestTools.java:656  Connection is broken: "javax.net.ssl.SSLHandshakeException:
          (certificate_unknown) PKIX path building failed …"
CRATONVM: TestTools.java:656  Connection is broken: "java.io.IOException:
          TLS handshake failed: …"
```

`TestTools.java:656` is `getConnection("jdbc:h2:ssl://localhost:9001/mem:")`.
H2's `CipherFactory.setKeystore()` sets `javax.net.ssl.keyStore` and **never a
trust store**, so on a modern JDK the client cannot validate its own server's
self-signed certificate. Upstream H2 does not see this because
`testServerMain` guards `testSSL()` behind `if (!config.ci)`. Both VMs now
reach the same line and reject the same certificate; only the exception type
and the time-to-report differ, which is tracked separately as
`bug-cratonvm-tls-client-handshake-failure-reported-as-interrupted-20260807.md`.

## 3. `TestMemoryUnmapper` — not a VM defect

`TestMemoryUnmapper.test()` line 53 is the THIRD assertion:

```java
pb.command(getJVM(), "-cp", getClassPath(), "-ea",
        "-Djava.security.manager", "-Dh2.nioCleanerHack=true", className);
assertEquals(UNAVAILABLE /* 2 */, pb.start().waitFor());
```

It expects the child JVM to *run* and answer "unmapping unavailable under a
security manager". JEP 486 removed the Security Manager in JDK 24, so
`-Djava.security.manager` makes the launcher fail and the child exits 1.
Stock HotSpot 25.0.3, same classpath:

```
org.h2.test.unit.TestMemoryUnmapper Expected: 2 actual: 1
  at org.h2.test.TestBase.assertEquals(TestBase.java:506)
  at org.h2.test.unit.TestMemoryUnmapper.test(TestMemoryUnmapper.java:53)
```

Identical class, identical line, identical numbers. Note also that `getJVM()`
is `java.home + /bin/java`, so all three child processes are the real JDK
regardless of which VM runs the parent — there is nothing CratonVM-specific
in this test's outcome at all.

Unrelated to the `nioMapped:` unmap-timeout write-up despite the shared
`MemoryUnmapper` name: that one is a GC-timing issue in `TestFileSystem`, with
a timeout; this is a synchronous count check in a different class.

## 4. `TestFileLock.testSimple` — `Files.createFile` was not `CREATE_NEW`

**Root cause.** `native_files_create_file` used `std::fs::File::create`, i.e.
`O_CREAT|O_WRONLY|O_TRUNC`. `Files.createFile` is specified as `CREATE_NEW`:
it is the atomic create-if-absent primitive of `java.nio.file`, so callers use
it AS a lock rather than merely to make a file. The implementation got both
halves wrong at once — it reported success on an existing path AND emptied it.

`FlProbe.java` against H2's own `org.h2.store.fs.FileUtils`:

```
                    HOTSPOT   CRATONVM(before)
createFile(new)     true      true
createFile(again)   false     true              <-- silent success
Files.createFile on existing:
                    FileAlreadyExistsException   NO EXCEPTION
```

H2's `FilePathDisk.createFile` catches `FileAlreadyExistsException` to answer
"another process already holds this lock file". With a silent success, `lock2`
skipped the whole already-in-use branch of `FileLock.lockFile`, fell through to
`save(); sleep(25); load()`, and threw `ERROR_OPENING_DATABASE_1`
("Concurrent update", `FileLock.java:337`) where `testSimple` asserts
`DATABASE_ALREADY_OPEN_1`. The 0-second failure — against HotSpot's 17 s — is
the tell: the whole `waitUntilOld()` + `sleep(2 * LOCK_SLEEP)` arm never ran.

Worth stating plainly: two H2 `FileLock`s both believed they had taken the
database lock, and the truncation had already destroyed the first holder's id
on disk. The test assertion was the mild symptom.

**Fix.** `native-io/src/lib.rs`: `OpenOptions::new().write(true).create_new(true)`,
with `AlreadyExists` mapped to a real `java.nio.file.FileAlreadyExistsException`
via the existing `nio_native::file_already_exists` builder and every other
errno through `io_err_nio`. The sibling create-if-absent paths were checked and
were already correct: `UnixFileSystem/WinNTFileSystem.createFileExclusively0`
(behind `File.createNewFile()`) and the `CREATE_NEW` open-option handling in
`fsp_scan_open_options` / `AsynchronousFileChannel.open`.

**Regression cover.** `regression-suite/src/RJdkNio.java` now asserts both
halves — the exception type, and that a refused `createFile` does not truncate
what is already there. `RJdkNio` PASSES against HotSpot with it.

**Verified.** `TestFileLock` RC=0 3/3 at 16 s (HotSpot: 17 s); it had failed
3/3 at 0–1 s.

## 5. `TestTimer.loop` — not a VM defect

`TestTimer` creates `TEST(ID IDENTITY, NAME VARCHAR)` and then executes
`INSERT INTO TEST VALUES(NULL, 'Hello')`. H2 2.x rejects an explicit `NULL`
into an identity column. Stock HotSpot 25.0.3, same classpath, 0 seconds:

```
Exception in thread "main" org.h2.jdbc.JdbcSQLIntegrityConstraintViolationException:
  NULL not allowed for column "ID"; SQL statement:
INSERT INTO TEST VALUES(NULL, 'Hello') [23502-249]
  at org.h2.table.Column.validateConvertUpdateSequence(Column.java:406)
  at org.h2.test.synth.TestTimer.loop(TestTimer.java:60)
```

Identical class, line, and message. (Separately, `TestTimer` is documented as
"an endless loop … usually stopped by switching off the computer", so it could
never PASS under a per-class timeout even if the insert worked.)

## Residuals handed on

* **TLS handshake-failure reporting** — a client handshake that fails on
  certificate validation surfaces as `IOException: TLS handshake failed: the
  handshake process was interrupted` after ~30 s, where HotSpot raises
  `SSLHandshakeException` naming the PKIX failure immediately. Filed as the
  `bug-cratonvm-tls-client-handshake-failure-reported-as-interrupted-20260807`
  page — which WAS created, worked on 2026-08-10, and retired as
  `bug-cratonvm-tls-client-handshake-reported-as-interrupted-20260807-FIXED-20260810`
  (note the dropped `failure-`, which is why the 2026-08-16 reopening below
  searched for it and concluded it had never existed).
* **`TestPgServer` throughput** — 687–809 s under CratonVM against 17 s on
  HotSpot for the same work. That is the known interpreter/native-call
  throughput wall, not a defect of this cluster.
* Two regression-suite vectors, `RSocketChannelInterrupt` and `RJdkHandles`,
  fail both before and after these changes — confirmed against a pristine
  `origin/dev` (`1082eb446`) binary, so they are not fallout from them.

## Repro (representative)
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.unit.TestFileLock   # or TestPgServer / TestTools
```
Always run the same class on `$JDK25/bin/java` with the identical classpath
first — it is what settled items 3 and 5 in minutes.


## Reopened 2026-08-16: fresh evidence for the untracked TestTools residual

`org.h2.test.unit.TestTools` FAIL, `origin/dev` (post `f80a4b775`), Azure host
`azureuser@20.80.105.49`, GC-sweep rerun (default/G1/ZGC, 1500s timeout):

```
org/h2/jdbc/JdbcSQLNonTransientConnectionException: Connection is broken:
  "javax.net.ssl.SSLHandshakeException: TLS handshake failed: the handshake
  process was interrupted: localhost:9001"
	at org/h2/test/unit/TestTools.main(TestTools.java:81)
	at org/h2/test/TestBase.testFromMain(TestBase.java:479)
	at org/h2/test/unit/TestTools.test(TestTools.java:103)
```

Compare against this doc's own §2b closing table:

```
HOTSPOT : TestTools.java:656  Connection is broken: "javax.net.ssl.SSLHandshakeException:
          (certificate_unknown) PKIX path building failed …"
CRATONVM: TestTools.java:656  Connection is broken: "java.io.IOException:
          TLS handshake failed: …"
```

The exception TYPE has moved from `java.io.IOException` (2026-08-07) to the
correct `javax.net.ssl.SSLHandshakeException` (today) — real, if
undocumented, progress. But the MESSAGE is still CratonVM's own generic
"the handshake process was interrupted" rather than HotSpot's actual PKIX
certificate-validation diagnostic. This is precisely the residual the
original doc flagged and promised to file separately; it was not filed, and
it is still open. Filing it now under this doc rather than a new one, since
the original promised filename was never claimed by anything else in the
tree and there is no reason to fragment the history further.

**What's still needed**: find where CratonVM's TLS client-handshake failure
path constructs its `SSLHandshakeException` (likely in the same
`SSLSocket.connect()`-adjacent code the original §2b fix touched — the parked
in `servlet::PENDING_CONNECT_SOCK_ID_BASE` handshake trigger points) and make
it surface the real certificate-chain-validation failure reason instead of a
generic "interrupted" message, matching HotSpot/JSSE's own
`CertificateException`-derived wording.

## Closed 2026-08-16: the residual was a server-side accept loop, not a client message

Everything below is MEASURED on `azureuser@20.80.105.49`, branch
`fix/h2-tls-zipfs-20260816` off `origin/dev` (`ecc09d40d`),
`--java-home /data/toolchain/jdk-25 --nojit --Xmx 1g`, one class per scratch
CWD, with a pristine build of the same commit as the control arm.

### The reopening's hypotheses, ruled out

`TlsProbe` — H2's `NetUtils` on both ends, ONE client connection, the shape the
2026-08-10 page used:

```
CRATONVM (pristine dev): javax.net.ssl.SSLHandshakeException: TLS handshake failed:
  error:0A000086:…:tls_post_process_server_certificate:certificate verify failed:
  … (EE certificate key too weak)                                    ELAPSED 57 ms
HOTSPOT                : javax.net.ssl.SSLHandshakeException: (certificate_unknown)
  PKIX path building failed: … unable to find valid certification path
                                                                     ELAPSED 128 ms
```

So on the unmodified binary the client already reported a real
certificate-verification failure, immediately, correctly typed. Neither the
message construction nor the `PENDING_CONNECT_SOCK_ID_BASE` trigger points can
be the fault — the reopening's two named suspects were both already correct.

### What the probe was missing: a second connection

`TcpServer.isRunning()` opens a loopback socket and closes it again **without
any I/O**, and `Server.start()` does that before the JDBC client ever connects.
`TlsProbe2` adds exactly that — three probe sockets, then a real client:

```
HOTSPOT                                     CRATONVM (pristine dev)
SERVER accepted                    x4       (nothing)
worker: SSLHandshakeException      x3       SERVER accept ended: java.io.IOException:
  "Remote host terminated the                 legacy DSA TLS server handshake:
   handshake"                                 the handshake failed: unexpected EOF
worker: SSLHandshakeException               (accept loop is gone)
  "(certificate_unknown) Received
   fatal alert: certificate_unknown"
REAL-CLIENT   149 ms  PKIX rejection        REAL-CLIENT  30 162 ms  "the handshake
                                                          process was interrupted"
```

`rustls_server_accept` did the TCP accept AND the full TLS handshake, and
returned the handshake failure as its error — which
`SSLServerSocket.accept()` turns into an `IOException`. H2's
`TcpServer.listen()` exits its accept loop on that, as any JSSE accept loop
would: JSSE does not run the handshake in `accept()` at all, so a handshake
error is not something an accept loop has any reason to expect. **H2 killed its
own SSL server on its own liveness probe.** The 30 s that followed is not a
client policy at all — it is the client's `SO_RCVTIMEO` expiring on a
connection sitting in a listen backlog nobody is accepting from, at which point
OpenSSL reports `WANT_READ` and `native_tls` renders it as
`HandshakeError::WouldBlock` — "the handshake process was interrupted".

This is also why the 2026-08-10 page, which fixed the *other* cause of a
30 s `WouldBlock` (one `SSLSocket` dialling two TCP connections), measured
605 ms and looked finished: its `TlsBoth` fixture makes exactly ONE client
connection, so there is no earlier failed handshake to kill the listener.

### Fix

`native-builtins/src/t27_tls.rs` — a failed handshake is carried ON the
accepted stream (`TlsServerStream::HandshakeFailed`) instead of being thrown
out of `accept()`. `accept()` returns a socket, as JSSE's does; the first
`read`/`write` on it raises `javax.net.ssl.SSLHandshakeException` with the
backend's real reason, which is where JSSE raises it. Plumbed through
`servlet::s2_tls_handshake_failure` and
`phases_late/ssl_security.rs`'s `tls_io_failure`, so all four Java-facing
TLS stream I/O natives report it the same way.

After, same `TlsProbe2` invocation:

```
SERVER accepted                                                          x4
SERVER worker: SSLHandshakeException: legacy DSA TLS server handshake:
  the handshake failed: unexpected EOF                                   x3
SERVER worker: SSLHandshakeException: … sslv3 alert bad certificate … alert number 42
REAL-CLIENT ms=36 -> javax.net.ssl.SSLHandshakeException: TLS handshake failed:
  error:0A000086:… certificate verify failed … (EE certificate key too weak)
```

Four accepts, three dead workers, one certificate rejection — HotSpot's shape,
line for line.

`org.h2.test.unit.TestTools`: **38 s → 4.9 s** (5.5 s on a re-run), still FAIL
at `TestTools.java:656`, which is where stock HotSpot 25.0.3 fails too (3.0 s),
for the reason §2b already gives: H2 sets `javax.net.ssl.keyStore` and never a
trust store.

Regression suite, both arms of the same branch: CORE 43 pass / 0 fail,
JDK-only corpus 28 pass / 0 fail. (`RSocketChannelInterrupt` and `RJdkHandles`,
which the "Residuals handed on" section above recorded as failing on
`1082eb446`, both pass on today's `dev` — fixed in between by other work.)

### One thing this page had wrong, now filed separately

§2b and the 2026-08-10 page both say "both VMs reject the same certificate;
only the report differs". Only the first half is true. H2's certificate is
`CN=H2`, RSA, **MD5withRSA**, self-signed. With H2's own keystore installed as
the trust store as well — so the certificate IS trusted — `TlsProbe3` gets:

```
HOTSPOT  : TRUSTED-HANDSHAKE-OK                                    197 ms
CRATONVM : REJECTED  SSLHandshakeException: … certificate verify failed
           … (EE certificate key too weak)                          50 ms
```

CratonVM's client applies OpenSSL's SECLEVEL to the peer certificate; HotSpot
applies the JDK's `jdk.certpath.disabledAlgorithms` rules, which exempt a trust
anchor from the signature-algorithm check. The two VMs agree on `TestTools`
only because H2 configures no trust store there. Filed as
`docs/known-issues/tls-client-trust-is-openssl-seclevel-not-the-jdk-trustmanager-20260816.md`
— it changes no verdict in this cluster, and it is a real divergence in the
other direction (CratonVM refusing what HotSpot accepts), so it does not belong
buried in a closed page.

## Related
* `bug-h2-testtransaction-merge-using-lock-timeout-RESOLVED-20260807.md` —
  re-verified 2026-08-16, still accurately closed (not reopened).
* `bug-cratonvm-tls-client-handshake-reported-as-interrupted-20260807-FIXED-20260810.md`
  — the other half of the same 30 s symptom, fixed six days earlier.
