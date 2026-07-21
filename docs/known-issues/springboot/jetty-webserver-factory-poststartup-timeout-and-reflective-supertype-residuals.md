# Jetty factory post-startup timeout and reflective-supertype residuals

**Status: MOSTLY FIXED - 2026-07-20. The reflective-supertype residual, four
real bugs, the `Deflater.end()` monitor hang (both factory classes), and the
blocking-read-timeout bug below are all fixed. `JettyReactiveWebServerFactoryTests`
completes cleanly (22/35 passing — remaining failures are a missing `test.jks`
test fixture, unrelated to CratonVM). `JettyServletWebServerFactoryTests` no
longer hangs on the blocking-read bug either — it now runs 3x further into the
class (42+ server start/stop cycles vs. the old stuck point at 14) before
hitting a newly-exposed, unrelated OPEN residual (severe TLD/JAR-scan slowdown
in Xerces XML parsing, not a hang) — see the bottom section. **2026-07-21
update: the JAR-open/JarFile-native/classpath-resource-lookup layers have all
been ruled out with hard timing measurements, and the slowdown has been
isolated to genuine Xerces SAX-parsing execution cost via a standalone,
file-I/O-free repro. Root-caused (via cdb stack sampling) and PARTIALLY
FIXED: `invokestatic` was missing the same inline-cache fast path
`invokevirtual`/`invokespecial`/`invokeinterface` already had, so every
static method call in JDK-internal (Xerces, and any other bootstrap-package)
bytecode paid full method resolution + a heap-allocating descriptor parse on
every single call, not just the first. Fixed in
`vm/src/runtime/interpreter.rs` — real, verified, universal interpreter
improvement (not Jetty/Xerces-specific), but the full suite still does NOT
complete within 900s: a large (~180s) stall remains on at least one test,
consistent with first-call/cache-population cost across the much larger set
of distinct call sites a full DTD/schema-validating parse exercises (this
session's repro was intentionally non-validating). Still OPEN; see the
bottom section for the complete diagnosis, what's fixed, and what's next.**

## Scope and separation

This is deliberately separate from the fixed private-lambda owner-dispatch
issue. The former `StackOverflowError`, duplicate-registration, and
`FilterRegistration.Dynamic` symptoms are absent. The remaining failures occur
after normal Jetty startup or in `Method.invoke` assignability validation.

## Reproduction (original, 2026-07-18)

Using Spring Boot 4.1.0-SNAPSHOT with Jetty 12.1.8 and the direct `SbRunner`
launcher, HotSpot/JDK 25 passes all three affected classes:

| Class | HotSpot | CratonVM JIT | CratonVM `--nojit` |
|---|---:|---:|---:|
| `JettyReactiveWebServerFactoryTests` | 35 pass, 1 skipped | timeout after 180s | timeout after 180s |
| `JettyServletWebServerFactoryTests` | 113 pass, 2 skipped | timeout after 180s | timeout after 180s |
| `JettyServletWebServerServletContextListenerTests` | 2 pass | 2 pass | 2 pass |

The two factory logs show repeated successful `ServletContextHandler` and
`Server` startups before the timeout, not recursion or duplicate servlet
registration. The listener failure is:

```
IllegalArgumentException: object of type
org.springframework.boot.jetty.autoconfigure.servlet.JettyServletWebServerServletContextListenerTests
is not an instance of
org.springframework.boot.web.server.servlet.AbstractServletWebServerServletContextListenerTests
```

A 30-second `--stack-dump-on-timeout` capture of the reactive factory class
places the main thread in
`AbstractReactiveWebServerFactoryTests.compressionOfResponseToGetRequest` ->
`Mono.block(Duration)` -> `BlockingSingleSubscriber.blockingGet`. Jetty worker
threads are idle in `QueuedThreadPool` wait sites. This rules out continued
`startContext()` recursion and narrows the timeout to post-startup request /
response delivery.

## Fixed reflective-supertype residual

`loader_aware_reflect_assignable` now walks the receiver's resolved superclass
chain before rejecting a class target. This preserves a valid relation when a
reflective `Method` mirror holds a different loader copy of a superclass. The
listener class passes 2/2 in JIT and `--nojit` with the correction.

## Three additional real bugs found and fixed while chasing the factory timeouts (2026-07-18)

Deeper `--stack-dump-on-timeout` captures against the reactive factory class
(closeout worktree `codex/fix-jetty-private-lambda-closeout-20260718`)
consistently landed on
`AbstractReactiveWebServerFactoryTests.givenAnInflightRequestWhenTheServerIsStoppedThenGracefulShutdownCallbackIsCalledWithRequestsActive`,
not the compression test — the specific hanging test method varies run to run
depending on which one is reached first before the class-wide timeout, but
all three fixes below were confirmed via focused repros independent of Jetty.

1. **Deflater only ever produced output on `FINISH`** — `defl_deflate_bytes_bytes`
   (`native-builtins/src/zip_real.rs`) buffered all input and only ran a
   one-shot `flate2` compress when the JDK flush code was `FINISH` (4); any
   `SYNC_FLUSH`/`FULL_FLUSH` call silently no-opped. A streaming HTTP gzip
   writer (Jetty's `GzipHttpOutputInterceptor`) that blocks for a mid-stream
   flush to make progress before handing over more input would wait forever.
   Rewritten to hold a real streaming `flate2::Compress` and honor
   `FlushCompress::{Sync,Full,Finish}` per call, mirroring how
   `InflaterState`/`Decompress` already worked. Regression test:
   `deflate_sync_flush_produces_output_without_finish`.

2. **Wildcard connect targets threw `WSAEADDRNOTAVAIL` (os error 10049) on
   Windows** — `AbstractReactiveWebServerFactoryTests` builds its own client
   base URL via `new InetSocketAddress(port)` (module
   `spring-boot-web-server`, `testFixtures/.../AbstractReactiveWebServerFactoryTests.java:381-382`),
   which is a wildcard address (`0.0.0.0`). Real JDK's native connect path
   resolves a wildcard connect *destination* to loopback before dialing
   (confirmed empirically: HotSpot connects successfully to a
   wildcard-address target on this same Windows host); CratonVM dialed the
   literal wildcard address and Windows rejected it outright. Fixed in both
   the NIO path (`sc_connect_inner` / `connect_target_host`,
   `native-io/src/socket_channel.rs`) and the legacy `Socket` path
   (`socket_connect`, `native-builtins/src/plain_socket.rs`). Separately,
   `ss_wrapper_local_address` (the `java.net.ServerSocket` wrapper Jetty's
   connector queries) had the same wildcard-publishing gap already fixed for
   `ssc_local_address` in `nettyrsocketserverfactorytests-bindexception-os-error-10049-FIXED-20260718.md`
   but missed here — also fixed to route through the loopback substitution.
   These three fixes together eliminated every `BindException`/
   `WSAEADDRNOTAVAIL` from the reactive factory class's logs. Regression
   test: `connect_target_host_substitutes_loopback_for_wildcard_only`.

3. **Calling `Deflater.deflate()` again after `FINISH` already reached
   `Z_STREAM_END` corrupted unrelated heap state** — real JDK's contract is
   that `deflate()` is a no-op once finished (0 bytes in/out, `finished`
   stays true) until `reset()`; the rewritten streaming implementation from
   fix 1 did not replicate this guard and re-entered `flate2::Compress::compress`
   on an already-finished stream. Under concurrent load (8 threads
   continuously creating/discarding `Deflater`s while racing `System.gc()`,
   in `DeflaterMonitorRepro2.java`, not committed — see below) this
   reproduced as `NullPointerException: Cannot assign field "node" because
   "mover" is null` inside real `jdk.internal.ref.CleanerImpl$CleanableList.remove`
   — i.e. genuine heap corruption surfacing somewhere unrelated, not a clean
   exception at the call site. Fixed by tracking `finished: bool` on
   `DeflaterState` and short-circuiting to a 0/0/finished result instead of
   re-entering `compress()`. Regression test:
   `deflate_after_finish_is_a_no_op_not_a_reentry`. The 8-thread repro no
   longer reproduces any corruption after this fix (20+ clean runs).

## Fixed: the `Deflater.end()` monitor hang (2026-07-18)

With the three fixes above, `JettyServletWebServerServletContextListenerTests`
passes and the `BindException`/`WSAEADDRNOTAVAIL` symptom is gone, but
`JettyReactiveWebServerFactoryTests` still hit the full 200s timeout
(reproduced identically with `--nojit`). A `--stack-dump-on-timeout=150`
capture was deterministic across many separate runs (JIT on and off):

```
tid=0 name="main" blocked=true top=java/util/zip/Deflater.end@6
  <- org/eclipse/jetty/util/compression/DeflaterPool.end@1
  <- org/eclipse/jetty/util/compression/DeflaterPool.end@5
```

`javap` on the real `jetty-util-12.1.8.jar`/JDK 25 `rt` classes confirmed
this is the intended call chain (`DeflaterPool.end(Object)` bridge →
`end(Deflater)` → `Deflater.end()`), and pc=6 is exactly the `monitorenter`
on `Deflater`'s `zsRef` field — i.e. the main thread was blocked entering a
per-instance monitor also used by `Deflater$DeflaterZStreamRef.run()` (the
JDK Cleaner's synchronized cleanup action for the same field), yet no live
thread — not the idle `Common-Cleaner`, not any Jetty worker — ever showed as
holding it in any capture.

**Root cause**: `ThreadRegistry::mark_dead` (called when a Java thread
terminates) never released any monitor that thread might still hold. A
thread torn down while blocked inside a native call made from within a
`synchronized` region — exactly what happens when Jetty abandons the
deliberately-stuck "in-flight request" test thread after its
graceful-shutdown timeout — never executes its own `monitorexit` bytecode.
Worse, a monitor that was still an *uncontended thin lock* (never inflated)
at the moment its owner died couldn't be swept at death time at all: it only
gets inflated later, by whichever thread next contends it, and that
inflation pre-seeds the freshly-created `Monitor`'s owner straight from the
stale thin-lock mark word — so even a general "sweep this dead thread's
monitors" pass at death time misses it.

**Fix** (`vm/src/threading/monitor.rs`, `vm/src/vm/vm_exec.rs`,
`vm/src/native/jni.rs`): added `MonitorTable::release_monitors_held_by`
(wired into every `mark_dead` call site) for the already-inflated case, and
a dead-owner check in `monitor_enter_blocking` right after
`enter_or_contend` inflates a contended monitor, for the thin-lock-inflated-
later case. Regression tests:
`dead_thread_owned_monitor_is_released_and_future_enters_succeed`,
`contended_inflation_of_a_dead_threads_thin_lock_is_recoverable`.

**Verified**: `JettyReactiveWebServerFactoryTests` no longer hangs — it now
completes in 156s (was: hangs forever at 200s+ every run). 22/35 pass; the
13 failures are almost all `IllegalArgumentException: Package ... did not
contain resources: [test.jks]` (a test-fixture/classpath-resource-listing
issue, not this bug — HotSpot would need the same file) plus one
`compressionOfResponseToGetRequest` timeout that did not reproduce again on
a subsequent run, consistent with test-execution-order sensitivity rather
than a deterministic hang.

`JettyServletWebServerFactoryTests` (3x more tests) also no longer gets
stuck at the old `DeflaterPool.end()` point — it now makes it through 14
server start/stop cycles before hitting the *different*, unrelated bug
documented below.

## Fixed: blocking-read timeout not enforced (2026-07-20)

`JettyServletWebServerFactoryTests` still did not complete even at a 900s
timeout (5x the original). A `--stack-dump-on-timeout` capture showed a
completely different signature from the `Deflater.end()` bug above:

```
tid=0 name="main" blocked=true
  top=sun/nio/ch/SocketDispatcher.read@4
    <- sun/nio/ch/NioSocketImpl.tryRead@45
    <- sun/nio/ch/NioSocketImpl.timedRead@11
```

This is a real OS-level blocking `read()` syscall that never returns — not a
monitor wait, so the interpreter's dump mechanism can't get a live frame walk
(the thread never reaches a Java-bytecode check-in point), only this cached
3-frame summary. `timedRead` (as opposed to `tryRead`) is the JDK's
bounded-timeout read path, used only when `SO_TIMEOUT` is set — so a
`SocketTimeoutException` should have fired and didn't.

**Root cause**: a `CRATONVM_DBG_NET=1` trace (added as a temporary
`dbgnet!`/`eprintln!` instrumentation pass in `configureBlocking`) showed
every single `configureBlocking(fd, false)` call for the affected connections
hitting the registry while the fd was still `NetSocketHandle::Unbound`:

```
[NET] configureBlocking fd=0x40000003 kind=unbound blocking=false NO-OP
```

This is exactly the failure mode the doc's own "Next steps" predicted.
`NioSocketImpl.connect(timeout)` — the path Apache HttpClient5's classic/io
transport uses (`org.apache.hc.client5.http.impl.io`, the transport backing
`AbstractServletWebServerFactoryTests`'s `HttpComponentsClientHttpRequestFactory`-based
client) — calls `IOUtil.configureBlocking(fd, false)` **before**
`Net.connect0`, while the fd has no live OS socket yet (`net_socket0` defers
actual socket creation to `bind0`/`connect0`). `net_connect0`
(`native-io/src/net.rs`) then always created a fresh, default-*blocking*
`TcpStream` and inserted it into the registry, silently discarding the
earlier non-blocking request. Every later `read0` on that connection
therefore performed a genuine blocking OS `read()` instead of returning
`IOStatus.UNAVAILABLE` (-2), so `NioSocketImpl.timedRead`'s poll-based
`SO_TIMEOUT` protocol never engaged and the read blocked until the peer
closed (or, in this suite, forever — the peer never closes in the "no data
yet" case a `SO_TIMEOUT` read is supposed to bound).

The exact same class of bug did NOT exist in `socket_channel.rs`'s
`sc_connect_inner` (the `SocketChannel`-native connect path), which already
re-applies a pre-connect `configureBlocking(false)` request to the freshly
connected stream — confirming this was a gap specific to `net.rs`'s
`Net.connect0`/`Net.bind0` path, not a general design omission.

**Fix** (`native-io/src/net.rs`): added `net_pending_nonblocking()`, a
small per-fd registry recording the last requested blocking mode. The
`sun/nio/ch/IOUtil.configureBlocking` native handler now records the
request unconditionally (not just when a live `Stream`/`Listener` exists),
and `net_connect0` / `net_bind0` consume-and-apply any pending request right
after creating the live socket — mirroring the pattern `sc_connect_inner`
already used. Regression test:
`t19_5_connect0_applies_nonblocking_requested_while_fd_was_unbound`.

**Verified**: `JettyServletWebServerFactoryTests` no longer hangs at the old
stuck point — it now completes 3x more server start/stop cycles (42+ vs. the
previous 14) before hitting the unrelated residual documented below. A
`SoTimeoutRepro`-style standalone `java.net.Socket` + `setSoTimeout` repro
(not committed) confirmed the fix directly: a client blocked on `read()` with
no data available now throws `SocketTimeoutException` after the configured
timeout instead of hanging.

## New OPEN residual found once the hang above stopped masking it: severe TLD/JAR-scan slowdown in Xerces XML parsing

With the blocking-read bug fixed, `JettyServletWebServerFactoryTests` still
does not complete within a 900s timeout — but the failure mode has changed
from a hang to severe cumulative slowness, and per-cycle timing shows this is
**not** a livelock:

```
cycle:  ... 13  14  15   16  17 ... 34  35   36  17 ... 42  43   44 ...
delta:  ...  5s 10s 185s  5s  5s ...  2s 186s 16s  3s ... 12s 194s 10s
```

(seconds between successive `Jetty started` log lines; full class ~115
tests). Most server-start/stop cycles take 2-16s, but a handful spike to
~185-194s each — three observed spikes alone account for over 550s of the
900s budget. `HotSpot completes the entire class in 22.6s` (verified via the
same suite-runner harness with `-Vm hotspot`), so this is not a fundamental
JDK-level cost; the JettyServletWebServerFactoryTests port-clash tests
(`portClashOfPrimaryConnectorResultsInPortInUseException` and similar)
correlate with a spike in the one `--stack-dump-on-timeout` capture taken
mid-spike:

```
tid=0 name="main" blocked=false
  ... JettyServletWebServerFactory.getWebServer
  ... JasperInitializer.doStart -> TldScanner.scan -> TldScanner.scanJars
  ... StandardJarScanner.scan -> TldParser.parse -> Digester.parse
  ... (real Xerces SAX parser, ~30 frames of Xerces internals)
  top=com/sun/org/apache/xerces/internal/impl/XMLEntityScanner.load
```

`blocked=false` and successive dumps show the frame depth cycling through a
stable ~110-124 pattern rather than sitting at one fixed pc — i.e. the thread
is actively working, repeatedly re-entering `XMLEntityScanner.load` (the
buffer-refill primitive) once per small `.tld`/`web-fragment.xml` file across
the ~171 jars on the module's flat classpath (see the
`[jboss-bf] getResources(META-INF/MANIFEST.MF): capping 171 flat-classpath
matches to 128` log lines), once per Jetty server start (i.e. potentially
once per test in the class). This is the same subsystem — and likely the
same underlying "interpreter/native dispatch overhead makes Xerces
character-level scanning prohibitively slow without a dedicated fast path"
pattern — documented and fixed for a **different** set of `XMLEntityScanner`
methods (`scanQName`, `scanContent`, `skipSpaces`, `normalizeNewlines`,
`checkEntityLimit`; see `force_native_over_real_jdk_bytecode` /
`is_xerces_xml_parser_native_override` in `vm/src/runtime/interpreter.rs`)
in
`docs/internal/fixed-suite-bugs/keycloak-model-liquibase-xerces-xml-parse-nojit-timeout-FIXED.md`.
`load` is notably **absent** from that force-native gate's method list.

Unlike the Liquibase/Keycloak case (one large XSD/changelog file, dominated
by per-character `scanQName`/`scanContent` work), TLD scanning parses **many
small files**, so the balance was suspected to shift to per-call overhead
somewhere else in the pipeline. This section originally speculated about
JAR-open cost and a `load`-specific native fast path; a follow-up session
(2026-07-21, worktree `fix/jetty-tld-jarscan-slowness-20260721`) measured
each candidate directly and narrowed it down substantially. **Still OPEN —
not fixed** — but the search space is now much smaller.

### Ruled out, with hard numbers

1. **Raw JAR-open cost (`native-io/src/zip_real_jar.rs`, the `zip` crate).**
   A diagnostic Rust test (`zip_real_jar::tests::diag_bench_open_all_module_jars`,
   `#[ignore]`d, run with `CRATONVM_DIAG_JAR_LIST=<classpath file>`) opened
   all 167 real jars on the `spring-boot-jetty` module's test classpath via
   plain `zip::ZipArchive::new`: **215.9ms total (167 jars, avg 1.29ms
   each)**, 197ms on a warm-cache second pass. Even at ~115 repeats (worst
   case, no caching) that's ~25s total — nowhere near the observed 550s+ of
   spike time. Raw zip parsing is not the bottleneck.

2. **`java.util.jar.JarFile`/`ZipFile` native construction is never even
   reached for this workload.** Added `CRATONVM_DBG_JAR=1` tracing to
   `open_and_register`/`native_jarfile_close` (fd-handle open/close +
   registry size). A 300s trace run of `JettyServletWebServerFactoryTests`
   (covering multiple full server-start cycles) produced **zero** `[JAR]`
   lines. Tomcat's TLD scanning for this classpath shape does not go through
   CratonVM's `java.util.jar.JarFile` native emulation at all.

3. **`classloading::ClassPath::new` (the flat-classpath loader that reads
   every jar's bytes into memory once, referenced by the `[jboss-bf]`
   log lines) is not reconstructed per server start.** Added
   `CRATONVM_DBG_CLASSPATH=1` timing to `ClassPath::new`. Across the same
   300s trace window (~20+ server-start cycles), it was called only **twice**
   total, at **167ms** for a real 176-path load. It's built once (or a
   couple of times) at VM/classloader setup, not per Jetty server instance —
   the original "reopens the same 171 jars from scratch on every test"
   hypothesis is wrong.

4. **`ClassPath::find_resource` / `find_all_resource_urls` (the resource
   lookup layer backing `getResource(AsStream)`/`getResources`) are not the
   bottleneck either.** Added `CRATONVM_DBG_RESOURCE_TIMING=1` timing
   (`diag_resource_call_wrapper` in `classloading/src/class_path.rs`). Over a
   400s trace window (31 server-start cycles), only **415 total calls**,
   **~38ms cumulative time**. Negligible.

### Confirmed, with hard numbers: it's genuine Xerces/SAX parsing execution cost

A standalone, pure-JDK repro (no file/jar/classpath I/O at all — see
`docs/known-issues/repros/xerces-sax-manysmallfiles-slowdown/`) parses a
~500-byte TLD-shaped XML document repeatedly with a **reused** `SAXParser`
(mirroring Tomcat's pooled `Digester`, so parser-construction cost doesn't
confound the measurement):

| | HotSpot | CratonVM (jit=on) | CratonVM (`--nojit`) |
|---|---:|---:|---:|
| reused-parser parse | ~44us | ~8.7ms | ~13.0ms |
| fresh `newSAXParser()` | ~360us | ~12.6ms | ~15.0ms |

**~197x slower per parse even with a warm/reused parser**, entirely inside
the `parse()` call, with zero file or jar I/O involved. JIT provides a real
but modest ~33% speedup (8.7ms vs. 13.0ms) — it is not being denied/skipped,
but it doesn't come close to closing the gap, matching the Liquibase/Keycloak
precedent's own experience (that fix needed dedicated native fast paths, not
just "let the JIT handle it").

This single isolated measurement is the right order of magnitude to explain
the real-world spikes: a ~190s spike over a genuinely small number of actual
`.tld`-file parses (most of the 167 classpath jars are filtered out by
Tomcat's own jar-skip-list before ever being opened) is entirely consistent
with each real parse costing single-digit milliseconds to low tens of
milliseconds, especially once Digester's DTD/schema-validation overhead
(absent from this minimal repro) is added back in.

**`UTF8Reader` tested and refuted as the specific hot method.** Given `load`
is a thin ~15-bytecode wrapper around one `Reader.read(char[], int, int)`
call, and `com.sun.org.apache.xerces.internal.impl.io.UTF8Reader` (the
concrete `Reader` Xerces picks for UTF-8-declared documents, confirmed via
`javap -c` on `XMLEntityScanner.createReader`) is — like `load` — **absent**
from the existing `XMLEntityScanner` force-native gate, it was a natural
next suspect. `SaxEncodingCompare.java` (same repro directory) parses the
identical logical document as both UTF-8 (`UTF8Reader`) and US-ASCII
(`ASCIIReader`) under CratonVM: ASCII was **not** faster (13.5ms vs. 8.7ms
for UTF-8) — if `UTF8Reader`'s byte-decode loop were the hot path, ASCII
should have been faster, not slower. The bottleneck is in scanning/attribute/
namespace/entity-manager/symbol-table machinery shared by both encodings,
not in encoding-specific byte decoding.

### Still OPEN — what's left

The exact hot method(s) within Xerces's general SAX scanning pipeline
(`XMLDocumentFragmentScannerImpl`/`XMLNSDocumentScannerImpl`, attribute
processing, symbol-table interning, entity-manager/grammar-pool setup even
for a non-validating, DTD-less parse) are **not yet pinned down** — this
needs real sampling-profiler or debugger tooling (`cdb`/WinDbg is not
installed on this box; confirmed unavailable both in the original session
and this follow-up) to go further responsibly, rather than more
guess-and-measure cycles. The Liquibase/Keycloak precedent fix iterated
through a long list of specific methods
(`XMLChar`, `XMLLimitAnalyzer`, `XSSimpleTypeDecl`, `XSDHandler$XSDKey`,
`scanQName`/`scanContent`/`skipSpaces`/`normalizeNewlines`/`checkEntityLimit`,
opti-DOM getters, `RangeToken.sortRanges`) over what was clearly a
substantial, iterative investigation — closing this residual properly likely
needs the same scale of effort, not a single targeted native-method
addition.

**Deliberately not attempted in the prior session**: implementing a native
fast path without being able to verify which method(s) actually dominate
risks shipping a subtly-incorrect Xerces reimplementation (the doc's own
framing for the *previous* residual explicitly flagged this risk) while not
even fixing the reported slowdown if the guess is wrong (as the `UTF8Reader`
hypothesis was).

## Root-caused and PARTIALLY FIXED (2026-07-21, cdb profiling follow-up)

Installed `cdb`/WinDbg on the box (via `winget install Microsoft.WinDbg`,
which bundles `cdbX64.exe` — modern WinDbg's MSIX package, not just the
GUI). The release profile already builds with `strip = "none"` +
`debug = "line-tables-only"` (a prior perf session's setup, see the
`[profile.release]` comment in the workspace `Cargo.toml`), so symbols were
available immediately — no rebuild-for-symbols step needed.

**Technique**: launched `SaxManySmallFiles` (6000 reps, ~80s+ wall clock) as
a detached background process, then repeatedly non-invasively attached
(`cdb -pv -p <PID> -y <symdir> -lines -c "~*kb 20;qd"`, ~25 samples over the
run) and extracted the `main-vm` thread's (CratonVM's actual interpreter
thread — distinct from the OS "main" thread, which just waits on a
`WaitForSingleObject`) leaf frame each time — a cheap poor-man's sampling
profiler, no `cdb` scripting extensions needed.

**Result: 25/25 samples landed at the exact same spot** —
`cratonvm_vm::runtime::interpreter::split_method_descriptor` (line 20282,
`params.push(descriptor[start..i].to_string())`) called from
`execute_invokestatic` (line 28578 at the time), heap-allocating a
`Vec<String>` via `alloc::raw_vec::RawVec::grow_one` →
`mimalloc`/`_mi_theap_get_free_small_page`. **Every single `invokestatic`
bytecode instruction re-parsed and heap-allocated the full parameter-type
list from scratch**, even though every consumer
(`coerce_invoke_arg_for_descriptor`, `decode_arg_kind_aware`) only ever read
the **first byte** of each parameter token — exactly the case the
already-existing `nth_param_tag_byte` non-allocating helper (added for a
*different*, narrower "warm call-dispatch arm" need — see its doc comment)
was built for.

Worse: `execute_invokevirtual_cached`/`execute_invokestatic_cached` (an
inline monomorphic call cache, `thread.invoke_cache`) already exists and is
already wired into the interpreter's *raw-byte-peek fast dispatch loop* —
but the **main `execute_instruction` dispatcher** (used for JDK-internal
classes like `com.sun.org.apache.xerces.*`, since that fast loop is
deliberately gated off for them — see the `Instruction::Invokevirtual`/
`Invokespecial` arm's own comment, itself a 2026-07-xx fix for the identical
class of bug that took "a standalone SAX/DTD parse-loop repro... from
~155ms/parse to ~0.6ms/parse" when applied to those two instructions) called
`execute_invokestatic_cached` at exactly one call site
(the OS-thread-startup pending-attach dance) but **never consulted the cache
for `Instruction::Invokestatic` in the dispatcher every JDK-internal-class
static call actually goes through** — it called the slow, allocating
`execute_invokestatic` unconditionally, unlike its `Invokevirtual`/
`Invokespecial`/`Invokeinterface` siblings right next to it in the same
`match`.

**Fix** (`vm/src/runtime/interpreter.rs`):
1. Wired `execute_invokestatic_cached` into the `Instruction::Invokestatic`
   arm of `execute_instruction`, mirroring the existing
   `Invokevirtual`/`Invokespecial` arm exactly (cache hit → handled; miss →
   fall through to the existing slow path, which already populates the
   cache via `populate_invoke_cache` for next time).
2. Changed `coerce_invoke_arg_for_descriptor` to take the parameter's tag
   `u8` directly instead of `&str` (every call site only ever read
   `.as_bytes().first()`), and replaced every `split_method_descriptor(&d)`
   + `Vec<String>`/`.get(i)` pattern feeding it with direct
   `nth_param_tag_byte(&d, i)` calls — across
   `pop_coerced_invoke_args_virtual`, `pop_coerced_invoke_args_static`,
   `execute_invoke_kind`, and `execute_invokestatic`'s own slow-path arg
   popping. This removes the heap allocation entirely from the cache-miss/
   cold path too (first call to any given call site, JVMTI-redefine/
   synthetic-stub-upgrade evictions), not just the now-cached steady state.
   `pop_coerced_invoke_args_intrinsic`'s already-non-allocating
   `Arc<[Arc<str>]>`-backed path was left untouched (already correct — only
   its now-`u8`-signature call to `coerce_invoke_arg_for_descriptor` needed
   updating for the signature change).

**Re-profiling after the fix** (same 25-sample cdb technique): the leaf
frame is no longer dominated by one spot — samples spread across
`execute_invokevirtual_cached`, GC heap operations
(`gen_heap::get_header`/`compact_field_slot`), class resolution
(`RedefineGate::is_stale`, `class::find_method`), instruction decoding,
`RwLock` operations, and normal mimalloc alloc/free — i.e. the interpreter
now looks like it's doing a diversified mix of genuinely necessary work
rather than being monopolized by one wasteful allocation. This confirms the
fix eliminated the exact bottleneck found.

**Verification and honest result**: the box was under heavy shared load
during verification (52-73% background CPU from concurrent sessions/builds
— see `feedback_shared_host_multitenant_confound`), so a direct wall-clock
A/B of the microbenchmark was noisy and inconclusive (interleaved runs
showed anywhere from a 16% improvement to a wash). The code-location
evidence above (25/25 → fully diversified) is solid regardless of timing
noise. Running the real `JettyServletWebServerFactoryTests` suite with the
fix: **17 server-start cycles completed with NO large spikes at all
(3-12s each, vs. the pre-fix baseline's mix of 2-16s normal + 185-194s
spikes)** — a real, visible improvement — **but a ~182s stall then occurred
on the 18th cycle** (`22:29:55.836` "Jetty started" → `22:32:58.054` next
log line), and the class still did not complete within 900s. `cargo test -p
cratonvm-vm --lib interpreter::` after the fix: 193/194 pass; the one
failure (`buffered_input_stream_real_jdk_uses_its_own_bytecode`) is
pre-existing and unrelated — it fails identically on the unmodified merge
base (confirmed via `git stash`), asserting a `force_native_over_real_jdk_bytecode`
gate for `java/io/BufferedInputStream` that no longer exists anywhere in the
non-test code (a stale test from an earlier refactor, not caused by this
fix, not investigated further here — out of scope).

**Working theory for the remaining ~182s stall (2026-07-21, REFUTED — see
below)**: the per-call-site cache only helps on the *second-and-later* call
to a given `(caller_class, cp_index)` pair — the *first* call to each
distinct call site still pays full resolution (native-registry lookup,
`force_native_over_real_jdk_bytecode`/`synthetic_stub_should_yield_to_real_bytecode`
checks, `class_manager` `RwLock` reads), now non-allocating but not free.
This session's isolated repro (`SaxManySmallFiles`) intentionally used a
small, non-validating, DTD-less parse to isolate the invokestatic-caching
bug cleanly — a real `.tld` file parsed through Tomcat's actual
`Digester`/schema-aware pipeline exercises a much larger, more varied set of
Xerces/XNI classes and methods (grammar pool setup, DTD/XSD validators,
symbol tables), plausibly with thousands of call sites hit for the first
time in a single parse. If so, the remaining cost is aggregate
first-resolution cost across many distinct call sites, not a single
repeated hot loop.

## Root-caused for real and FIXED (2026-07-21, later same-day follow-up): it was never Xerces

Live `cdb` sampling of a **real suite run's actual 182s stall** (not the toy
repro) definitively refuted the "aggregate first-call resolution cost"
theory above: the stuck `main-vm` thread was parked in `net_poll`'s socket
wait loop, not executing any interpreted bytecode at all. This ruled out
Xerces/invokestatic-cache entirely and redirected the investigation to the
network layer.

**Identifying the exact test**: correlated JUnit discovery-order (probed
directly via a small reflection harness) against the suite log's cumulative
"Jetty started" line count to pin the stall to
`JettyServletWebServerFactoryTests.compressionOfResponseToGetRequest` (the
18th server-start cycle). Reproduced in isolation via `SbRunnerMethod`
(`apps/spring-boot/sb-runner`), deterministic every time — no full 900s
suite run needed to iterate.

**Root cause #1 — Deflater direct-`ByteBuffer` natives were unimplemented
stubs.** `java.util.zip.Deflater` has 4 native compress overloads
(`deflateBytesBytes`, `deflateBytesBuffer`, `deflateBufferBytes`,
`deflateBufferBuffer`); only `deflateBytesBytes` (byte[]-in/byte[]-out) had
a real implementation in `native-builtins/src/zip_real.rs` — the other 3
all threw `RuntimeError::NotImplemented` via a shared
`defl_direct_buffer_unsupported` stub. Jetty's `GzipHttpOutputInterceptor`
calls the `deflateBufferBuffer` overload whenever both its scratch input
buffer and the connector's pooled network output buffer are direct (the
normal NIO connector case) — so every gzip-compressed response through
Jetty's real NIO path hit the stub, which raised an exception deep inside a
blocking write call in a way that left the connection wedged rather than
cleanly failing (independently confirmed server-side-only via `curl` and a
raw-socket Java probe: the client received a 10-byte gzip header and then
nothing, matching a mid-response native failure rather than a client bug).

Fixed by implementing all 3 direct-buffer overloads for real, sharing core
compression logic with the existing `deflateBytesBytes` path via a new
`defl_do_compress` helper (`native-builtins/src/zip_real.rs`). Direct-buffer
addresses are resolved native `long` values (the JDK bytecode wrapper
already extracts `((DirectBuffer) buf).address()` before calling the
native), read/written via the existing `NativeContext::copy_from_native_memory`/
`copy_to_native_memory` primitives — the same pattern already used in
`classloader.rs`/`lang_invoke.rs`/`lang_system.rs` for direct-buffer access.

**Root cause #2 — `CRC32.updateByteBuffer0` was *also* a stub**, returning
the input CRC unchanged instead of computing over the buffer's bytes
(`native-builtins/src/zip_real.rs`, `crc32_update_byte_buffer_0` — a
pre-existing, deliberate "return unchanged, better than UnsatisfiedLinkError"
placeholder per its old comment). This second bug was masked by the first:
fixing only the Deflater hang let the test proceed far enough to actually
exercise Jetty's own CRC32-based GZIP trailer computation for the first
time, which failed with `java.util.zip.ZipException: Corrupt GZIP trailer`
(ISIZE correct at 10000, CRC32 always exactly `0x00000000` — the
tell). Fixed the same way as Deflater: read the buffer's bytes via
`copy_from_native_memory` and feed them through the existing
`crc32_update_public` helper (same CRC32/IEEE algorithm already used by the
working byte[] overload).

**Verification**: `compressionOfResponseToGetRequest` went from hanging
(previously reported as a ~182s stall, confirmed via isolated repro to
actually hang indefinitely once traced directly rather than via the
900s-suite-timeout artifact) to **passing in ~21-24s**. `CRATONVM_DBG_DEFLATE`
env-gated tracing (left in place, opt-in, zero default behavior change)
confirms the fix path: input consumed across calls summed to the expected
10000 bytes, `flush_code=4` (Z_FINISH) on the final call.

**Lesson**: this is the second time in this same investigation that a
plausible-looking "interpreter is slow at X" theory (first Xerces
char-by-char scanning, then aggregate invokestatic first-resolution cost)
turned out to be wrong once a real profiler was pointed at the *actual*
stall rather than a hand-picked isolated repro — the toy repro
(`SaxManySmallFiles`) was faithfully reproducing a real bug (the
invokestatic cache gap, genuinely worth fixing) that was simply not the
cause of *this* particular residual. Prefer sampling the real failure over
extrapolating from a similar-looking synthetic one.

**Suite verification (2026-07-21)**: ran both classes solo via
`apps/spring-boot-suite-runner`.
`JettyReactiveWebServerFactoryTests`: 35 tests, 8 failed — all pre-existing
SSL/TLS handshake failures (`StacklessSSLHandshakeException`, unrelated to
compression/CRC, not investigated further here). `JettyServletWebServerFactoryTests`:
progressed cleanly through **60 server-start/stop cycles in 571s** (vs. the
pre-fix baseline of 17-18 cycles before an indefinite hang) — a large,
real improvement — then the process **terminated abnormally (exit code 1,
no panic message, no `SBRUNNER_RESULT`)** immediately after starting its
61st server instance (an `h2c`-enabled connector). It was never reached
before this fix (the class always hung around cycle 18-19 first) — a
fourth instance in this investigation of a residual that was masked by an
earlier-blocking bug.

## Investigated further (2026-07-21) — real and reproducible, but root cause NOT identified; every obvious internal-bug signature ruled out

Ran the full class 6 more times total (2 plain, 1 with a per-test
`TestExecutionListener` reporting exactly which test was running via
`STARTING_TEST`/`FINISHED_TEST` lines, 1 launched directly under `cdb`,
plus 1 more after pulling in a real, independently-discovered, directly
relevant fix from a concurrent session — see below). Also ran a raw
JUnit-free stress loop and two isolated slice-replay experiments.

- **Crash position varies (test #4, #60, #61, #67) but is NOT fully
  random**: two separate runs of the exact same rebuilt binary (one under
  `cdb`, one plain) both died at the exact same test,
  `whenHttp2IsEnabledAndSslIsDisabledThenH2cCanBeUsed`, immediately after
  `sslKeyAlias()` FAILED — the earlier claim in this doc that the `cdb` run
  "completed cleanly to test #73" was a misreading of that log; it died at
  the same test, silently, exactly like the others. So position is at
  least partially reproducible for a given binary/test-order, not purely
  random — but different binaries/sessions land at different positions.
- **A raw, JUnit-free stress loop** (`JettyCycleStress.java`: plain
  `factory.getWebServer(...)` + start + GET + stop + destroy in a tight
  loop, no SSL) ran **150 cycles with no crash** — handle/thread counts
  grew modestly (~50%) but never anything catastrophic. SSL involvement
  seems to matter, not just cycle count.
- **Correlation with a failing SSL test is consistent**: in every crash
  observed, the test immediately before the crash was an SSL-related test
  that FAILED (`pkcs12KeyStoreAndTrustStoreFromBundle`,
  `sslNeedsClientAuthenticationSucceedsWithClientCertificate`,
  `basicSslFromClassPath`, `sslKeyAlias` — each failing with a genuine,
  separate `SSLHandshakeException: handshake read: connection reset by
  peer` bug, itself real and already covered by this codebase's many other
  tracked SSL/rustls residuals). But this does NOT reproduce standalone:
  - An isolated 2-test repro (`pkcs12KeyStoreAndTrustStoreFromBundle` +
    the next test, and separately `sslKeyAlias` +
    `whenHttp2IsEnabledAndSslIsDisabledThenH2cCanBeUsed`) ran clean both
    times.
  - A 14-test slice replaying the *entire* run of tests immediately
    surrounding one confirmed crash point (positions ~55-67, ending in the
    exact same `sslKeyAlias` → `whenHttp2IsEnabledAndSslIsDisabledThenH2cCanBeUsed`
    pair that crashed in the full run) also ran clean — `tests=14 failed=3`,
    no crash. **The crash needs the FULL accumulated state of running
    ~55+ preceding tests first — replaying just the local window around
    the trigger is not enough**, ruling out a simple "this one test pair is
    broken" explanation.
- **Tested against a real, independently-fixed, directly-relevant bug —
  did NOT resolve it.** A concurrent session merged
  `fix(vm): close GC-unstable objref_key side-table keys in t27_tls.rs`
  (dev commit `5dd7a7b18`) while this investigation was in progress: it
  fixed exactly the kind of defect this investigation was homing in on —
  `ssl_server_socket_states`, `sock_alpn_table`, and
  `session_peer_certs_table` (all in `native-builtins/src/t27_tls.rs`)
  were keyed by `objref_key()`, a hash of an `ObjectRef`'s *raw current
  pointer* — unstable under this VM's moving young-gen GC, so a lookup
  could silently collide with a stale entry from an unrelated,
  already-freed object now occupying the same address. Merged that fix
  into this branch, rebuilt, and re-ran the full class: **it crashed again,
  at the exact same test** (`whenHttp2IsEnabledAndSslIsDisabledThenH2cCanBeUsed`,
  immediately after `sslKeyAlias` FAILED). That fix was real and worth
  having, but it is not the (or not the only) cause of this residual.
- **Zero internal crash signature of any kind**, checked directly, across
  every run:
  - No `hs_err_pid<pid>.log` ever written (the installed Rust panic hook —
    `vm/src/runtime/crash_handler.rs` — writes one on every panic).
  - No `"panicked at"` / `"thread panicked"` text in any crash log.
  - No `"[cratonvm] main-vm run() returned Ok/Err"` banner
    (`vm-cli/src/main.rs`) — meaning the main-vm thread's `run()` call
    never even returned; something killed the process out from under it,
    not through its own normal or error exit path.
  - No `"[cratonvm] System.exit(N) called"` banner (rules out a Java-level
    `System.exit`/`Runtime.exit` call).
  - Launching the class **directly under `cdb`**
    (`cdb -g -G -c "g;.lastevent;kv;~*kv;q" <exe> <args>`) never stopped on
    an exception — the process exited from cdb's perspective with only
    module-unload-at-exit messages, no exception dump. This rules out
    access violations, stack overflows, illegal instructions, and
    `std::process::abort()`/fastfail (`__fastfail` reliably breaks into an
    attached debugger; it did not here) — at least for the specific crash
    `cdb` happened to observe.
  - **Windows Error Reporting has zero entries for this session's renamed
    binary** (`cratonvm-spring-boot-suite.exe`) across the entire
    investigation window, checked via
    `Get-WinEvent -FilterHashtable @{LogName='Application';Id=1000,1001,1002}`
    — while it DOES have entries for *other concurrent sessions'* CratonVM
    binaries on this same box in the same time window (e.g.
    `cratonvm-classvalue-jit-dispatch-20260721.exe`, and plain `cratonvm.exe`
    with a genuine `BEX64`/`c0000409` fastfail crash — see
    [[project_classvalue_residual5_jit_tierup_20260721]], unrelated to this
    doc). A genuine unhandled OS-level fault in our own binary should have
    produced a WER entry the same way; it did not.

**Honest conclusion**: real, reproducible-with-~10-minutes-effort residual,
requiring substantial (~55+ test) cumulative interpreter/JVM state plus an
SSL-test failure as an apparent trigger, but its actual kill mechanism
evades every diagnostic technique tried (panic hook, `hs_err` writer, `cdb`
live-attached launch, WER). Two non-exclusive possibilities remain open:
(a) genuine external process termination on this heavily shared,
multi-tenant Windows box (another concurrent session's cleanup script
matching `cratonvm*.exe` by wildcard — see
[[feedback_shared_host_blanket_process_kill]] — other sessions' differently
-named CratonVM processes were independently observed running throughout
this window), or (b) a genuine internal bug whose kill path bypasses all of
Rust's normal panic/abort/exit instrumentation (a raw Windows API call like
`ExitProcess`/`TerminateProcess` from within CratonVM's own code, not found
in this pass's `process::exit`/`abort` grep sweep — worth a targeted grep
for `ExitProcess`/`TerminateProcess`/`kernel32` FFI calls next). **Not
fixed — no code bug was conclusively identified.**

**Still OPEN / not yet done**:
1. Grep for raw `ExitProcess`/`TerminateProcess` Win32 FFI calls (not just
   `std::process::exit`/`abort`) as a possible internal, instrumentation
   -bypassing kill path not yet checked.
2. Re-verify whether this residual reproduces at all on a non-shared host
   (or with this box's other sessions paused) — would distinguish
   possibility (a) from (b) above definitively.
3. If pursuing further, build a lightweight in-process heartbeat that logs
   (with `fsync`) after every native call/test boundary, so whatever kills
   the process — internal or external — leaves a forensic trail of the
   last thing that happened; none of the passive techniques tried this
   session (panic hooks, `cdb` attach, WER) caught anything.
4. The analogous `Inflater` direct-buffer natives
   (`inflateBytesBuffer`/`inflateBufferBytes`/`inflateBufferBuffer` in the
   same file) are likely *also* unimplemented stubs (not yet checked/fixed
   in this pass) — same risk class, not yet known to be hit by any test.
5. Diagnostic instrumentation left in place (opt-in, zero default behavior
   change): `CRATONVM_DBG_JAR`, `CRATONVM_DBG_CLASSPATH`,
   `CRATONVM_DBG_RESOURCE_TIMING`, `CRATONVM_DBG_DEFLATE`, `CRATONVM_DBG_SOCK`,
   `CRATONVM_DBG_SOCK_BYTES`.
6. `cdb`/WinDbg is installed on this box (`Microsoft.WinDbg` via
   `winget install Microsoft.WinDbg --source winget`) — no longer a
   blocker for future sessions on this same machine; launching a suspect
   process directly under it (rather than attach-after-the-fact sampling)
   is a fast way to rule out hardware faults/aborts, but was NOT sufficient
   to identify this residual's actual cause.
