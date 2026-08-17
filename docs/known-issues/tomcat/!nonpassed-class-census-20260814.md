# Tomcat suite — every non-passed class from the 2026-08-14 two-backend run

| | |
|---|---|
| **Status** | OPEN census. One entry per *cause*, covering all 64 classes that did not PASS. |
| **Run** | Complete 651-class Tomcat suite, 2026-08-14, Windows, `-Xmx2g`, real JDK, JIT on, 300 s per-class cap. Two backends: **G1** and **ZGC**, 4 shards each (8 runner invocations, `-Parallel 2`). |
| **Totals** | G1 598 PASS / 30 HANG / 14 FAIL / 6 CRASH / 3 NOSUMMARY · ZGC 605 PASS / 17 HANG / 20 FAIL / 6 CRASH / 3 NOSUMMARY |
| **Why this page** | The per-shard logs name 64 classes and nothing said which of them share a cause. Nine causes do. |

## TL;DR — the 64 classes are nine problems, and four groups are not VM defects

1. **4 classes are host pagefile exhaustion**, not defects — the VM could not reserve its own 2 GiB heap. Remove them from any tally.
2. **29 of 64 fail on exactly one backend.** With one shard per backend this run cannot separate "GC-specific" from "flaky"; the pagefile event proves the box was a variable.
3. **2 classes abort the process where they should throw `OutOfMemoryError`** — the clearest new defect on this page.
4. **3 classes share one interface-dispatch defect** (`AbstractMethodError ... has no Code attribute` on a JUnit 4 interface).
5. **2 classes share one JMX defect** (`NoSuchMethodError` on a JDK-internal virtual-thread scheduler).
6. **~13 classes are wall-clock**, not stuck — the documented VM-wide per-call cost against a 300 s cap.
7. **~10 classes are environmental** — HotSpot fails them identically on this host.
8. **2 classes are permanent by-design gaps** (rustls implements no TLS 1.2 renegotiation).
9. **2 real crashes** remain genuinely uninvestigated.

**Read §1 before quoting any number from this page.**

## 1. What this run cannot tell you

* **The box ran out of commit charge mid-run.** Between **15:12 and 15:25** three G1 classes died with `memory allocation of 2147483648 bytes failed` — exactly the `-Xmx2g` heap, with `out=0` bytes, so nothing executed — and a fourth named the cause outright: Windows error 1455, *"the paging file is too small to complete the operation"*. The same event killed G1 shard 1 outright (resumed; it then finished 160/163). Anything in that window is suspect, which is why §2 exists.
* **One shard per backend means no repeat.** A class that is HANG on G1 and PASS on ZGC has been run **once** on each. That is not a backend difference; it is one observation per side.
* **No HotSpot arm ran in this run** for most classes. Where this page says "HotSpot fails identically" it cites the prior records named in that section, not fresh evidence.
* **Generational was not run.** Its `[moving-young] fallback` throughput collapse is why it was dropped here — see !dohead-family-consolidated-history-20260813.md in this directory, and springboot/moving-young-fallback-turns-four-classes-red-20260810.md.
* **Two findings have FIXED records and still did not pass** — see §10.

## 2. Not a defect: host pagefile exhaustion (4 classes)

| class | G1 | ZGC | evidence |
|---|---|---|---|
| `TestHttpHeaderSecurityFilter` | CRASH | PASS | `memory allocation of 2147483648 bytes failed`, 15:12:39 |
| `TestAbstractAjpProcessor` | CRASH | PASS | same, 15:12:49 |
| `TestUriUtil40` | CRASH | PASS | same, 15:25:24 |
| `TestRemoteCIDRFilter` | CRASH | PASS | `Os { code: 1455 ... }` — paging file too small — while spawning a thread |

All four are G1-only, all four passed under ZGC, and all four produced **zero bytes of stdout**: the process never reached Java. 2147483648 is the heap reservation, not a test allocation. `TestRemoteCIDRFilter` is the same event caught at a different syscall.

**Do not investigate these as VM bugs.** Re-run on an idle box; expect PASS. The lesson belongs to the harness, not the VM: 8 concurrent runner invocations x 2 parallel forks x 2 GiB reservations oversubscribed this host's commit limit.

## 3. CORRECTED — the two `*LargeHeap` classes are §2 again, not an allocator defect

**This section originally claimed CratonVM aborts where it should throw `OutOfMemoryError`, and read the 12 GiB figure as a 6x miscomputation of a ~2 GiB array. Both halves were wrong.** The correction, 2026-08-17:

| class | G1 | ZGC | at `-Xmx12g` on an idle box |
|---|---|---|---|
| `org.apache.tomcat.util.buf.TestByteChunkLargeHeap` | CRASH | CRASH | **`OK (1 test)`, 2.6 s** |
| `org.apache.tomcat.util.buf.TestCharChunkLargeHeap` | CRASH | CRASH | **`OK (1 test)`** |

Three facts kill the original reading:

1. **12884901888 is not a computed array size — it is the requested heap.** `run-tomcat-suite.ps1` sets `$LargeHeapXmx = '12g'` and hands any class whose name matches `LargeHeap` its own `-Xmx12g`, precisely because the 2 GiB suite default OOMs them on *both* VMs by design. 12884901888 bytes is exactly 12 GiB. The "6x ratio" was a coincidence of round binary numbers (12 GiB really is 8 x 1.5 GiB) and meant nothing.
2. **The VM already throws a proper catchable OOM.** Run at `-Xmx2g` the same class fails cleanly, exactly as HotSpot would:
   ```
   java.lang.OutOfMemoryError: Java heap space (alloc_array length 1610612736)
     at org.apache.tomcat.util.buf.ByteChunk.makeSpace(ByteChunk.java:571)
     at org.apache.tomcat.util.buf.TestByteChunkLargeHeap.testAppend(...:41)
   ...
   Tests run: 1,  Failures: 1
   ```
   `Tests run: 1, Failures: 1` — a FAIL, not a CRASH. Nothing about Java-level allocation is broken.
3. **Both classes pass at the heap the harness actually asks for**, once the box can supply it. The suite CRASHes are timestamped 15:22:52, 15:22:57, 15:24:03 and 15:24:09 — *inside* the 15:12-15:25 commit-charge window §2 documents. A box that could not reserve 2 GiB at 15:12 could not reserve 12 GiB at 15:22.

**So these two classes belong to §2, and §2 covers 6 classes, not 4.** No VM defect, no harness change needed: the harness was already right to ask for 12 GiB, and asking for it while seven other shards are resident is what failed.

### What IS a defect, and it is the diagnostic

The entire user-facing output for an unsatisfiable heap reservation is the global allocator's abort:

```
$ cratonvm -Xmx400g ...
memory allocation of 429496729600 bytes failed
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
```

Empty stdout, rc=127, and no mention of `-Xmx`, of the heap, or of the machine. HotSpot says *"Could not reserve enough space for object heap"*. **That missing sentence is the whole reason this section was written wrong** — the line names a number and nothing else, so it reads as an internal allocator bug rather than "your `-Xmx` does not fit".

Fixed by making the reservation fallible (`gc/src/arena.rs::alloc_zeroed_heap`, used by `Arena::new` — the ZGC default, the generational young semi-spaces, `heap.rs` — and by G1's region array), which now reports:

```
# There is insufficient memory for the Java Runtime Environment to continue.
# Could not reserve enough space for object heap (arena)
#   requested 429496729600 bytes (409600 MiB) - this size comes from -Xmx
#   The OS refused the reservation. Either lower -Xmx, or free physical
#   memory / page-file (commit charge) on this machine and retry.
```

Only the reservation path changed. A Java-level allocation inside an existing heap still raises a catchable `OutOfMemoryError`, per fact 2 above.

## 4. One interface-dispatch defect: `Annotatable.getAnnotations()` has no Code attribute (3 classes)

| class | G1 | ZGC |
|---|---|---|
| `org.apache.tomcat.util.buf.TestMessageBytesConversion` | PASS | FAIL |
| `org.apache.catalina.servlets.TestDefaultServletEncodingWithBom` | HANG | FAIL |
| `org.apache.catalina.servlets.TestDefaultServletEncodingPassThroughBom` | HANG | FAIL |

```
java.lang.AbstractMethodError: method org/junit/runners/model/Annotatable.getAnnotations()
    [Ljava/lang/annotation/Annotation; has no Code attribute
  at org.junit.validator.AnnotationsValidator$AnnotatableValidator.validateAnnotatable(...)
  at org.junit.validator.AnnotationsValidator.validateTestClass(AnnotationsValidator.java:36)
  at org.junit.runners.ParentRunner.applyValidators(ParentRunner.java:157)
```

`Annotatable` is a JUnit 4 **interface** and `getAnnotations()` is abstract there, so "has no Code attribute" correctly describes the interface method and identifies the wrong method as having been selected. It fires inside JUnit's **class validator**, before any test body runs, which is why the whole class dies at `Tests run: 1, Failures: 1`.

### CORRECTED 2026-08-17: this is a JIT inline-cache miscompile, not a missing registration

This section originally attributed it to interface dispatch never binding the implementation. **That was wrong** — it is the JIT, and the bisect is unambiguous. All on `TestMessageBytesConversion`, standalone, one binary:

| arm | result |
|---|---|
| default (JIT on) | `AbstractMethodError` |
| `--nojit` | **`OK (864 tests)`** |
| `CRATONVM_JIT_DENY=org/junit/validator/AnnotationsValidator$AnnotatableValidator.validateAnnotatable` | **`OK (864 tests)`** |
| `CRATONVM_JIT_SP_INLINE_MIC=0` | `AbstractMethodError` (still) |
| `CRATONVM_JIT_SP_INLINE_PIC=0` | **`OK (864 tests)`** |
| `CRATONVM_JIT_SP_INLINE_MEGA=0` | **`OK (864 tests)`** |

So: the **compiled body of `validateAnnotatable`** is at fault (denying just that one method is sufficient), and within it the **polymorphic / megamorphic inline-cache path** is the mechanism — the monomorphic cache is innocent, since disabling it alone changes nothing.

Interface dispatch itself is fine. A probe calling `getAnnotations()` through the `Annotatable` interface on each of `TestClass`, `FrameworkMethod` and `FrameworkField` returns `OK` for all three on CratonVM, including `FrameworkMethod`/`FrameworkField`, whose implementation is reached past an abstract `FrameworkMember` that implements the interface without declaring the method.

**A minimal synthetic repro does NOT reproduce it** — an interface, an abstract class implementing it without declaring the method, five concrete receivers and a 400k-iteration generic `invokeinterface` loop gives the correct answer on both VMs. So the trigger is narrower than "interface site goes megamorphic", and it has not been isolated.

Leading hypothesis, unproven: the inline PIC/megamorphic cascade's **miss edge** reaches a call bound to the constant-pool target — which for an `invokeinterface` is `Annotatable.getAnnotations()` itself, an abstract method — instead of falling back to the resolving helper. That would explain why disabling either cache fixes it (the site then always uses the helper, which resolves against the receiver) while disabling only the monomorphic one does not. `InlineRefusal::GuardNotEmittable` records that the single-pass backend's "plain direct-call arm emits no receiver guard at all", which is the shape this hypothesis needs.

**Not fixed.** This path is on every interface dispatch in the VM, so a change here without the trigger isolated risks silent miscompiles far beyond these three classes. The safe interim lever is per-method: `CRATONVM_JIT_DENY=org/junit/validator/AnnotationsValidator$AnnotatableValidator.validateAnnotatable` makes all three classes pass.

## 5. One JMX defect: a JDK-internal virtual-thread scheduler method (2 classes)

| class | G1 | ZGC |
|---|---|---|
| `org.apache.catalina.loader.TestVirtualWebappLoader` | PASS | FAIL |
| `org.apache.catalina.webresources.war.TestHandlerIntegration` | PASS | FAIL |

```
Caused by: java.lang.NoSuchMethodError: 'void com.sun.management.internal
    .VirtualThreadSchedulerImpls$BoundVirtualThreadSchedulerImpl.lock()'
  at org.apache.tomcat.util.modeler.OperationInfo.getSignature(OperationInfo.java:134)
  at org.apache.tomcat.util.modeler.OperationInfo.getMBeanParameterInfo(OperationInfo.java:198)
  at org.apache.tomcat.util.modeler.ManagedBean.getMBeanInfo(ManagedBean.java:434)
  at org.apache.tomcat.util.modeler.BaseModelMBean.getMBeanInfo(BaseModelMBean.java:230)
```

Tomcat's own MBean modeler reflects over operation signatures, and doing so reaches a JDK-internal virtual-thread scheduler implementation this VM does not fully model. Both classes fail only under ZGC in this run, but nothing in that stack is collector-shaped — treat the single-backend showing as §1's one-observation problem, not as evidence about ZGC.

`getSignature` is a **reflective** walk, so the trigger is which MBeans happen to be registered rather than what the test asserts. That makes it a plausible latent cause under other JMX-touching classes too.

## 6. Individually diagnosed, one class each

| class | G1 | ZGC | what the log says |
|---|---|---|---|
| `TestEnvEntry` | HANG | HANG | `javax.naming.NamingException: Found a circular reference involving [java:comp/env/env-ent...]`, plus `The JNDI reference [lookup-invalid] was expected to be of t...`. A real JNDI resolution defect: a lookup chain the VM believes is circular and Tomcat does not. Both backends, so not collector-related. |
| `TestStartupIPv6Connectors` | FAIL | FAIL | `java.net.URISyntaxException: Illegal character in URI at index 38: http://[fe80:0:0:0:1b75:41d6:c80...`, 1 of 4 tests. An IPv6 literal is being built into a URI containing a character `URI` rejects; scope-id/zone handling is the first place to look (this tree has a fixed record of `Inet6Address` dropping the scope id — same area, opposite direction). |
| `TestOneLineFormatterPerformance` | FAIL | FAIL | `java.lang.IllegalArgumentException: Illegal date/time conversion argument`. **Named `Performance` but not a timing failure** — a `java.util.logging` formatter defect, and this tree already carries a four-defect JUL formatter cluster record. Do not file this under §7. |
| `TestSwallowAbortedUploads` | FAIL | PASS | `Tests run: 10, Failures: 3`; `AssertionError: Limited upload with swallow enabled generates client except...`. Request-abort/swallow semantics. |
| `TestManagerWebapp` | FAIL | FAIL | `IllegalStateException: Error starting child` plus `RuntimeException: Configuration failure in first run only`, 2–3 of 3. Matches the pre-existing manager-webapp JMX / `InetSocketAddress` residual record. |
| `TestHttp2InitialConnection` | FAIL | FAIL | `Tests run: 6, Failures: 4`, both backends, consistent. Not yet reduced to a signature — the most under-investigated FAIL on this page. |
| `TestHostConfigAutomaticDeployment*` (7 classes) | HANG | 4 PASS / 2 HANG / 1 FAIL | `IllegalArgumentException: The main resource set specified [...] is not valid`, and an `InaccessibleObjectException`. A **fixture/deployment-path** problem rather than a collector one; the 300 s HANGs sit on top of it. The four that PASS under ZGC are §1's one-observation caveat in its purest form. |

## 7. Wall-clock, not stuck — the per-call cost against a 300 s cap (~13 classes)

`TestCharsetCachePerformance`, `TestMethodPerformance`, `TestELParserPerformance`, `TestResponsePerformance`, `TestMapperPerformance`, `TestAsyncMessagesPerformance`, `TestGenerator`, `TestELInterpreterTagSetters`, `TestJspConfig`, `TestELInJsp`, `TestCachedResource`, `TestHttp2Section_8_2`, `TestDefaultServletRfc9110Section13`.

These are the same VM-wide per-call dispatch cost the perf records price at 85–92x on call-dense workloads, landing on classes that are already slow or already huge. Two specifics worth naming:

* **`TestHttp2Section_8_2` is volume, not a stall.** Its data provider exposes 6,658 cases, and its own fixing record ships `run-section82-shards.ps1` precisely because the full matrix was never meant to fit one un-sharded window. A 300 s cap cannot express it.
* **`TestAsyncMessagesPerformance`** is the subject of the RBC.6 WebSocket-send investigation, which closed it as *not* a JIT-admission problem; that record carries the experiment and the recommendation not to pursue it further.

The `*Performance` classes assert against timing thresholds, so on this VM they fail rather than hang when they do complete. **`TestOneLineFormatterPerformance` is the exception and belongs in §6** — read the message before filing anything here by name alone.

## 8. Environmental — HotSpot fails identically on this host (~10 classes)

| family | classes | why |
|---|---|---|
| `catalina.tribes.test.channel.*` | `TestDataIntegrity`, `TestMulticastPackages`, `TestUdpPackages`, `TestRemoteProcessException` | Multicast clustering does not work on this network; HotSpot fails the same methods. Covered by !tribes-multicast-family-still-environmental.md in this directory, which this run re-confirms on both backends. |
| `catalina.tribes.group.interceptors.*` | `TestTcpFailureDetector`, `TestNonBlockingCoordinator`, `TestEncryptInterceptorLargeHeap` | Same clustering substrate. All three PASS under ZGC and fail under G1 — see §1. |
| `tomcat.integration.httpd.*` | `TestChunkedTransferEncodingWithProxy`, `TestFullReverseProxy`, `TestRemoteIpValveWithProxy` | Need a real Apache `httpd` binary and a working proxy round-trip. Two are G1-HANG / ZGC-PASS; one is the reverse. |
| realm | `TestJNDIRealmIntegration` | Needs a reachable LDAP directory. G1 HANG, ZGC PASS. |

None of these is a CratonVM finding. They are listed so nobody re-triages them a fifth time.

## 9. Permanent by-design gaps (2 classes)

| class | G1 | ZGC | failing method |
|---|---|---|---|
| `org.apache.tomcat.util.net.TestSsl` | FAIL | FAIL | `testClientInitiatedRenegotiation[JSSE]` |
| `org.apache.tomcat.util.net.TestClientCert` | FAIL | FAIL | `testClientCertPostZero[JSSE]` |

rustls does not implement TLS 1.2 renegotiation — a deliberate CVE-2009-3555 / 3SHAKE mitigation, not a gap to close. Both classes fail at exactly "1 failure", the count the two fixing records (testssl-client-initiated-renegotiation-FIXED.md and 21-tls-handshake-enforcement-gap-FIXED.md) carve out as permanent residuals. **These will never go green and must not be counted as regressions.**

## 10. Two real crashes, and two FIXED records that did not hold

### Genuinely uninvestigated

| class | backend | signature |
|---|---|---|
| `org.apache.coyote.http11.TestHttp11Processor` | ZGC CRASH (G1 PASS) | `EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x...`, with a full native frame dump in its `.log.err` |
| `TestHttpServletDoHeadInvalidWrite0ValidWrite1023` | ZGC CRASH (G1 NOSUMMARY) | `fatal runtime error: the System allocator may not use TLS with destructors, aborting` |

The second is a **Rust-runtime** abort during thread teardown rather than a Java-level failure, and it is the more distinctive of the two — a thread-local with a destructor being touched after the allocator is gone. Neither was chased here.

One warning looks like a lead and is not. Both logs are full of `gc::guard: a non-reference value was stored into a slot the class declares as a REFERENCE — boxing it into an AUTO`. **It appears exactly 15 times in passing classes too** (checked across `catalina.core.*`, all PASS), so it is uniform startup noise on this fixture, not the crash cause. Do not attribute to it.

### Records that claim FIXED while the class still does not pass

* **`TestNonBlockingAPI`** — G1 NOSUMMARY (289.6 s), ZGC HANG (300 s). An internal record dated 2026-08-13 closes this as the ZGC fragmentation to OOM to `dispatchUncaughtException` double fault (zgc-nonblockingapi-fragmentation-oom-double-fault-hang-FIXED-20260813.md), and the binary used here was built from a dev containing it.
* **`TestWebSocketFrameClient` / `TestWebSocketFrameClientSSL`** — both HANG, both backends, 300 s. An internal record dated 2026-08-13 closes the message truncation as a string-intrinsic hole.

Either those fixes are incomplete, or these are different failures wearing the same class names. **A FIXED record beside a still-red class is a question, not a contradiction to argue away** — re-run these three individually on an idle box with a raised cap before touching either record.

## 11. The full 64

`PASS` in a column means that backend passed it; every row failed on at least one.

| class | package | G1 | ZGC |
|---|---|---|---|
| `TestHttpServletDoHeadInvalidWrite0ValidWrite1023` | `jakarta.servlet.http` | NOSUMMARY | CRASH |
| `TestHttpServletDoHeadInvalidWrite0ValidWrite1024` | `jakarta.servlet.http` | PASS | CRASH |
| `TestHttpServletDoHeadInvalidWrite0ValidWrite1025` | `jakarta.servlet.http` | NOSUMMARY | PASS |
| `TestHttpServletDoHeadInvalidWrite0ValidWrite511` | `jakarta.servlet.http` | PASS | CRASH |
| `TestHttpServletDoHeadInvalidWrite1024ValidWrite0` | `jakarta.servlet.http` | PASS | NOSUMMARY |
| `TestHttpServletDoHeadInvalidWrite1024ValidWrite1025` | `jakarta.servlet.http` | PASS | FAIL |
| `TestResponsePerformance` | `org.apache.catalina.connector` | FAIL | FAIL |
| `TestSwallowAbortedUploads` | `org.apache.catalina.core` | FAIL | PASS |
| `TestHttpHeaderSecurityFilter` | `org.apache.catalina.filters` | CRASH | PASS |
| `TestRemoteCIDRFilter` | `org.apache.catalina.filters` | CRASH | PASS |
| `TestVirtualWebappLoader` | `org.apache.catalina.loader` | PASS | FAIL |
| `TestManagerWebapp` | `org.apache.catalina.manager` | FAIL | FAIL |
| `TestMapperPerformance` | `org.apache.catalina.mapper` | FAIL | FAIL |
| `TestNonBlockingAPI` | `org.apache.catalina.nonblocking` | NOSUMMARY | HANG |
| `TestJNDIRealmIntegration` | `org.apache.catalina.realm` | HANG | PASS |
| `TestDefaultServletEncodingPassThroughBom` | `org.apache.catalina.servlets` | HANG | FAIL |
| `TestDefaultServletEncodingWithBom` | `org.apache.catalina.servlets` | HANG | FAIL |
| `TestDefaultServletEncodingWithoutBom` | `org.apache.catalina.servlets` | HANG | HANG |
| `TestDefaultServletRfc9110Section13` | `org.apache.catalina.servlets` | HANG | HANG |
| `TestHostConfigAutomaticDeploymentAddition` | `org.apache.catalina.startup` | HANG | HANG |
| `TestHostConfigAutomaticDeploymentDeleteB` | `org.apache.catalina.startup` | HANG | HANG |
| `TestHostConfigAutomaticDeploymentDeleteC` | `org.apache.catalina.startup` | HANG | HANG |
| `TestHostConfigAutomaticDeploymentDir` | `org.apache.catalina.startup` | HANG | PASS |
| `TestHostConfigAutomaticDeploymentModification` | `org.apache.catalina.startup` | HANG | HANG |
| `TestHostConfigAutomaticDeploymentUpdateWarOffline` | `org.apache.catalina.startup` | HANG | PASS |
| `TestHostConfigAutomaticDeploymentXmlExternalDirXml` | `org.apache.catalina.startup` | HANG | PASS |
| `TestHostConfigAutomaticDeploymentXmlExternalWarXml` | `org.apache.catalina.startup` | HANG | FAIL |
| `TestStartupIPv6Connectors` | `org.apache.catalina.startup` | FAIL | FAIL |
| `TestEncryptInterceptorLargeHeap` | `org.apache.catalina.tribes.group.interceptors` | HANG | PASS |
| `TestNonBlockingCoordinator` | `org.apache.catalina.tribes.group.interceptors` | HANG | PASS |
| `TestTcpFailureDetector` | `org.apache.catalina.tribes.group.interceptors` | FAIL | PASS |
| `TestDataIntegrity` | `org.apache.catalina.tribes.test.channel` | HANG | FAIL |
| `TestMulticastPackages` | `org.apache.catalina.tribes.test.channel` | FAIL | FAIL |
| `TestRemoteProcessException` | `org.apache.catalina.tribes.test.channel` | FAIL | FAIL |
| `TestUdpPackages` | `org.apache.catalina.tribes.test.channel` | FAIL | FAIL |
| `TestCachedResource` | `org.apache.catalina.webresources` | HANG | PASS |
| `TestHandlerIntegration` | `org.apache.catalina.webresources.war` | PASS | FAIL |
| `TestAbstractAjpProcessor` | `org.apache.coyote.ajp` | CRASH | PASS |
| `TestHttp11InputBuffer` | `org.apache.coyote.http11` | PASS | NOSUMMARY |
| `TestHttp11Processor` | `org.apache.coyote.http11` | PASS | CRASH |
| `TestHttp11ProcessorDoHead` | `org.apache.coyote.http11` | PASS | NOSUMMARY |
| `TestHttp2InitialConnection` | `org.apache.coyote.http2` | FAIL | FAIL |
| `TestHttp2Section_8_2` | `org.apache.coyote.http2` | HANG | HANG |
| `TestELInJsp` | `org.apache.el` | HANG | PASS |
| `TestELParserPerformance` | `org.apache.el.parser` | HANG | HANG |
| `TestGenerator` | `org.apache.jasper.compiler` | HANG | HANG |
| `TestJspConfig` | `org.apache.jasper.compiler` | HANG | PASS |
| `TestELInterpreterTagSetters` | `org.apache.jasper.optimizations` | HANG | HANG |
| `TestOneLineFormatterPerformance` | `org.apache.juli` | FAIL | FAIL |
| `TestEnvEntry` | `org.apache.naming` | HANG | HANG |
| `TestChunkedTransferEncodingWithProxy` | `org.apache.tomcat.integration.httpd` | HANG | PASS |
| `TestFullReverseProxy` | `org.apache.tomcat.integration.httpd` | HANG | PASS |
| `TestRemoteIpValveWithProxy` | `org.apache.tomcat.integration.httpd` | PASS | HANG |
| `TestByteChunkLargeHeap` | `org.apache.tomcat.util.buf` | CRASH | CRASH |
| `TestCharChunkLargeHeap` | `org.apache.tomcat.util.buf` | CRASH | CRASH |
| `TestCharsetCachePerformance` | `org.apache.tomcat.util.buf` | HANG | HANG |
| `TestMessageBytesConversion` | `org.apache.tomcat.util.buf` | PASS | FAIL |
| `TestUriUtil40` | `org.apache.tomcat.util.buf` | CRASH | PASS |
| `TestMethodPerformance` | `org.apache.tomcat.util.http` | HANG | HANG |
| `TestClientCert` | `org.apache.tomcat.util.net` | FAIL | FAIL |
| `TestSsl` | `org.apache.tomcat.util.net` | FAIL | FAIL |
| `TestWebSocketFrameClient` | `org.apache.tomcat.websocket` | HANG | HANG |
| `TestWebSocketFrameClientSSL` | `org.apache.tomcat.websocket` | HANG | HANG |
| `TestAsyncMessagesPerformance` | `org.apache.tomcat.websocket.server` | FAIL | FAIL |

## 12. Reproduction

```powershell
# one class, one backend
apps\tomcat-suite-runner\run-tomcat-suite.ps1 `
  -RunName probe -Exe <cratonvm.exe> -GcFlag:'-XX:+UseG1GC' -Parallel:1 `
  -Start <index-in-all-tests.txt> -Count 1

# the whole suite, one backend, one of four shards (what this page is)
apps\tomcat-suite-runner\run-tomcat-suite.ps1 `
  -RunName craton-tomcat-g1-shard1-20260814 -Exe <cratonvm.exe> `
  -GcFlag:'-XX:+UseG1GC' -Start:1 -Count:163 -Parallel:2
```

`-GcFlag` **must** use colon-bind (`-GcFlag:'-XX:+UseG1GC'`); a value starting with `-` is otherwise parsed as another parameter name.

Do not run 8 invocations at once against a 2 GiB heap unless the box has the commit charge for 16 GiB of reservations — that is what produced §2.

Results for this run live under `apps/tomcat/.suite/results/craton-tomcat-{g1,zgc}-shard{1,2,3,4}-20260814/real-jit/`, each with a `results.csv` plus per-class `<fqcn>.log` and `.log.err`.
