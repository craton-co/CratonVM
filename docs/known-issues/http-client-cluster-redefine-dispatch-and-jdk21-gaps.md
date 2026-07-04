# http.client bug cluster (12 classes) — fixes landed + residuals

## Status

**Mostly fixed** on branch `fix/http-client-cluster-azure` (off dev
46e56861, Azure host `victor@20.84.156.31`, worktree `~/wt-httpclient`).
Reproduced and iterated entirely on Linux + OpenJDK 21 (`/usr/lib/jvm/java-21-openjdk-amd64`)
since that's the only JDK on this host; the original bug report was captured
on Windows + JDK 25 via `apps/spring-suite-runner`. Two of the fixes below are
JDK-21-specific gaps that likely don't exist on JDK 25 at all (harmless
either way); everything else is platform/JDK-version independent. **Final
verification against the official Windows + JDK 25 harness has NOT been done
in this session** — recommended as a follow-up before considering this fully
closed.

## Root causes fixed

0. **`jdk/internal/misc/PreviewFeatures.isPreviewEnabled()Z` missing** — blocked
   every class's JUnit launcher/discovery on JDK 21 before a single test method
   ran. Independently rediscovered while reproducing this cluster, but already
   fixed on dev by the time of merging (see
   `docs/internal/fixed-suite-bugs/keycloak-previewfeatures-ispreviewenabled-native.md`)
   — no code change needed from this branch; this session's duplicate
   registration was dropped at merge time.

1. **`JavaLangAccess.defineClass` bridge ran real bytecode instead of the
   native `ClassLoader.defineClass` path** — `jdk.internal.reflect.ClassDefiner`
   (used by `ReflectionFactory.newConstructorForSerialization`, i.e. Objenesis's
   default mock-instantiation strategy) calls this to define
   `GeneratedSerializationConstructorAccessorN`. The bridge used
   `ctx.invoke_virtual` to reach `ClassLoader.defineClass`, which runs REAL
   bytecode (`preDefineClass`'s `checkName`) instead of the registered native
   (`cl_define_class_pd`) that ordinary bytecode `invokevirtual` call sites hit
   first — surfaced as `NoClassDefFoundError: IllegalName: ...` because the
   internal (slash-form) name reached `checkName`, which rejects any `/`.
   Fixed by calling `classloader::cl_define_class_basic` directly instead of
   through `invoke_virtual`. native-builtins/src/shared_secrets_bridge.rs,
   native-builtins/src/classloader.rs (`cl_define_class_basic` → `pub(crate)`).

3. **`is_prohibited_package_name` blocked `jdk/internal/reflect/*`** — the
   privileged-package-spoofing guard's `PROHIBITED_PREFIXES` included
   `"jdk/internal"`, but real JDK 21 permits `ClassDefiner`'s throwaway
   `DelegatingClassLoader` (a non-bootstrap loader) to define classes there
   (verified empirically against real HotSpot). Added a `jdk/internal/reflect/`
   exemption alongside the existing `sun/reflect/misc/` one.
   classloading/src/class_manager.rs.

4. **`JavaLangAccess.getConstantPool(Class)` missing** — ByteBuddy's class-file
   reader (used by Mockito's inline mock maker to redefine a mocked JDK class)
   calls this; without it every `mock()` of a JDK class failed with
   `MockitoException: Could not modify all classes`. Thin passthrough to the
   existing `Class.getConstantPool()` native. native-builtins/src/shared_secrets_bridge.rs.

5. **JDK 21's `StackStreamFactory$AbstractStackWalker.callStackWalk` overload
   missing** — the registry had the pre-21 (no continuation params) and JDK-25
   (split `int,int` mode) overloads, but not JDK 21's `(long mode, int skip,
   ContinuationScope, Continuation, int batch, int startIndex, Object[])`
   shape. Broke `StackWalker.walk(...)`, including Mockito's `LocationImpl`
   (called on every mocked-method invocation). native-builtins/src/lang_stackwalker.rs.

6. **`JavaLangAccess.start(Thread, ThreadContainer)` missing** — structured-
   concurrency thread-pool plumbing (`SharedThreadContainer.start`, used by
   Jetty's thread pool) calls this; CratonVM doesn't model thread containers,
   so it's a passthrough to a real `Thread.start()`. native-builtins/src/shared_secrets_bridge.rs.

7. **THE core bug — inline-cache population never checked whether an ancestor
   class had been JVMTI-redefined.** Mockito's inline mock maker mocks a
   *concrete* class (e.g. `java.net.HttpURLConnection`) by redefining that
   class directly (weaving advice into its methods) and instantiating a
   trivial marker subclass that does **not** itself override every mockable
   method (unlike an interface mock's generated subclass). So the call
   receiver is that marker subclass — never redefined itself — while an
   ancestor up the hierarchy (`HttpURLConnection`) is the one actually
   redefined. Two of the three "does a native shadow this call" implementations
   in the interpreter only checked the *receiver's* redefine generation, never
   walking the ancestor chain's redefine status:
   - `populate_virtual_invoke_cache`'s ancestor walk had a documented
     "Round 19" exception (if a parent has BOTH bytecode and a native, native
     wins — used for e.g. `LinkedHashMap` overlay natives) that doesn't
     account for the bytecode having been *woven* by a redefine. This poisoned
     the inline cache with a `VirtualNative` target on the method's *second*
     call at a given call site — the first call (slow path) correctly ran the
     woven bytecode.
   - `try_stackless_invoke`'s ancestor walk had no such exception (bytecode
     always wins if present), but for a method whose real bytecode lives on a
     *further* ancestor than where our native is registered (e.g.
     `getInputStream` — real bytecode is on `URLConnection`, but our native is
     registered directly on `HttpURLConnection`), it found the native on the
     nearer, redefined class before ever reaching the real bytecode — so even
     the *first*, uncached call was wrong.
   Fixed by adding the same ancestor-redefine guard (`native_shadow_suppressed_by_redefine`
   equivalent, inlined) that `execute_invokevirtual_vtable_fast` already had
   in both remaining walks. Also added an analogous "does the *receiver's own*
   class already provide bytecode" short-circuit to
   `execute_invokevirtual_vtable_fast`'s ancestor walk for consistency with
   `populate_virtual_invoke_cache`'s existing "Round 63" guard (not confirmed
   as load-bearing for this specific bug, but a real gap in the same family;
   harmless, gated, gets exercised in the JIT-on / warm-cache path).
   vm/src/runtime/interpreter.rs. **Verified no regression**: built a variant
   with just this fix reverted (everything else identical) and diffed a
   20-class random sample of spring-web tests outside `http.client` —
   byte-identical succ/fail counts in every class, confirming the new guards
   are inert unless a class was actually JVMTI-redefined.

## Verified impact (Linux + JDK 21, `apps/spring-suite-runner`-equivalent harness)

| Class | Before (bug report, JDK 25/Windows) | After (this session, JDK 21/Linux) |
|---|---|---|
| `JettyClientHttpRequestFactoryTests` | FAIL (StackOverflowError) | 5/6 OK |
| `SimpleClientHttpResponseTests` | FAIL (IllegalArgumentException) | 4/5 OK |
| `ReactorClientHttpRequestFactoryTests` | ABEND | 2/10 (rest blocked, see below) |
| `reactive.ReactorClientHttpConnectorTests` | ABEND | 2/5 (rest blocked, see below) |
| `SimpleClientHttpRequestFactoryTests` | FAIL (AssertionFailedError) | 6/10 (rest pre-existing, see below) |

## Residuals — NOT fixed this session

### A. Linux-JDK-only NIO/socket gaps (expected absent on Windows/JDK 25)

Three DIFFERENT Linux-only JDK classes surfaced as missing natives, each
cascading into `NoClassDefFoundError` for later references once the owning
class enters the JVM's permanent "erroneous" state:

- `jdk/net/LinuxSocketOptions.incomingNapiIdSupported0()Z` — **fixed** (stub
  returns false; native-builtins/src/lib.rs), unblocks `jdk.net.Sockets.optionSets`.
- `sun/nio/ch/NativeThread.supportPendingSignals0()Z` — **NOT fixed**. Blocks
  `sun.nio.ch.UnixDispatcher.<clinit>` → cascades to
  `NoClassDefFoundError: org/apache/hc/client5/http/impl/io/DefaultHttpClientConnectionOperator`,
  breaking `BufferingClientHttpRequestFactoryTests` (0/7),
  `HttpComponentsClientHttpRequestFactoryTests` (0/11),
  `InterceptingStreamingHttpComponentsTests` (0/6).
- `sun/nio/ch/EPollSelectorImpl` (epoll-based `Selector`, Linux-only — Windows
  uses a completion-port/different selector class entirely) — **NOT fixed**.
  Blocks Netty's `NioIoHandler.openSelector`, breaking most of
  `ReactorClientHttpRequestFactoryTests` (8/10) and
  `reactive.ReactorClientHttpConnectorTests` (3/5).

None of `UnixDispatcher`/`NativeThread`/`EPollSelectorImpl` are loaded at all
on a Windows JDK — these three classes are almost certainly a Linux-host
testing artifact, not real bugs the Windows-based bug report would hit. Did
not implement a full epoll-backed `Selector`/Unix-dispatcher stub set since
that's a large, speculative undertaking for a platform the target environment
doesn't use — recommend re-checking these three classes' status on a Windows
build before spending more effort here.

### B. Genuine hangs — 4 classes, root cause NOT found

`JdkClientHttpRequestFactoryTests`, `OutputStreamPublisherTests`,
`SubscriberInputStreamTests`, `reactive.ClientHttpConnectorTests` all hang
(90s+ timeout, no completion) even after all fixes above. Chased one lead —
suspected NIO keep-alive/connection-reuse bug in `java.net.http.HttpClient` —
with minimal standalone probes (`ProbeJdkHttp.java`/`ProbeJdkHttp2.java`/`ProbeJdkHttp3.java`,
not preserved) exercising a raw loopback `ServerSocket` + real `HttpClient`,
including sequential requests over a reused connection: **all passed cleanly**
(no hang), so the actual failure is something specific to these tests'
combination of MockWebServer + these particular request patterns, not a
general keep-alive bug. `--stack-dump-on-timeout` only produces a native
dispatch-trace ring buffer (all interpreter threads were in native/Rust code,
not interpreted bytecode, when the watchdog fired) showing MockWebServer/okio
buffer churn — inconclusive without further `CRATONVM_DBG_SOCK`-style tracing.
Not investigated further due to time; worth a dedicated session with the
probe-based diagnosis approach used for the historical Jetty NIO bug (see
memory `jetty-nio-client-directbytebuffer-putbyte-bug` for the playbook).

### C. `SimpleClientHttpResponseTests::shouldNotCloseConnectionWhenResponseClosed` — 1/5, order-dependent

Consistently the ONE failure (4/5 pass) in this class:
`org.mockito.exceptions.misusing.UnfinishedVerificationException` thrown from
the *next* test method's `mock()` call (`MockingProgressImpl.mockingStarted`
detects a dangling unfinished verification left by the previous test). Could
NOT reproduce standalone: a probe replicating the exact sequence
(`given(getResponseCode)`, `given(getInputStream)`, read+drain+close the
stream, `verify(mock, never()).disconnect()`) across 6 iterations in a single
process, and separately with the exact byte-for-byte test body logic, both
passed cleanly every time. The real test's difference is JUnit 5's per-method
reflective `Constructor.newInstance()` instantiation (a fresh test-class
instance per `@Test`) — not replicated in the standalone probes. Suspect a
narrow residual in how Mockito's `MockingProgress` thread-local interacts with
JUnit's reflective instantiation path, possibly still touching the same
redefine-shadow family fixed above for a method/path not covered. Not
resolved this session.

### D. `SimpleClientHttpRequestFactoryTests` — 4/10, pre-existing/out-of-scope

`prepareConnectionWithRequestBody`, `deleteWithoutBodyDoesNotRaiseException`
("URL not set" — a *direct* `HttpURLConnection` via `URL.openConnection()`,
not a mock; unrelated to the redefine-dispatch family above),
`httpMethods`, `interceptor` (`Status code '0'...`) — already flagged as
pre-existing, out-of-scope synthetic-`HttpURLConnection` gaps in memory
`huc-setdooutput-wrong-slot-drops-post-body` (fails on the frozen baseline
too). Not addressed this session.

## Reproduction

```bash
# Azure host, worktree ~/wt-httpclient, branch fix/http-client-cluster-azure
ssh -i ~/.ssh/azure.pem victor@20.84.156.31
cd ~/wt-httpclient && cargo build --release -p cratonvm-cli -j8
TESTCP=$(cat /opt/cratonvm/apps/spring-framework/spring-web/build/cratonvm-testcp.txt)
CP="/opt/cratonvm/apps/spring-suite-runner:$TESTCP"
KRUN_STACK=1 ./target/release/cratonvm --java-home /usr/lib/jvm/java-21-openjdk-amd64 \
  -cp "$CP" KRun org.springframework.http.client.SimpleClientHttpResponseTests
```
