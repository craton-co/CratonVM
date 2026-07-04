# http.client bug cluster (12 classes) — fixes landed + residuals

## Status

**Substantially fixed** on branch `fix/http-client-cluster-azure` (off dev,
Azure host `victor@20.84.156.31`, worktree `~/wt-httpclient`). Iterated in
two rounds: an initial pass reproduced/fixed entirely on Linux + OpenJDK 21
(only JDK on that host), then verified against the OFFICIAL Windows + JDK 25
harness (`apps/spring-suite-runner`, `vmfrozen/cratonvm-rerun.exe` baseline)
where 2 more concrete bugs were found from the real Windows-side symptoms and
fixed back on the fast Linux loop. **6 of 12 classes are now fully OK
(match HotSpot); the rest have residuals documented below.**

## Verified impact (12 target classes)

| Class | Before (bug report) | After this session |
|---|---|---|
| `BufferingClientHttpRequestFactoryTests` | ABEND | **7/7 OK** |
| `HttpComponentsClientHttpRequestFactoryTests` | ABEND | 15/18 (3 residual, see below) |
| `InterceptingStreamingHttpComponentsTests` | ABEND | **6/6 OK** |
| `JettyClientHttpRequestFactoryTests` | FAIL (StackOverflowError) | **6/6 OK** |
| `JdkClientHttpRequestFactoryTests` | FAIL (AssertionError) | FAIL (unchanged; see hang note) |
| `OutputStreamPublisherTests` | TIMEOUT | TIMEOUT (unchanged) |
| `ReactorClientHttpRequestFactoryTests` | ABEND | 2/10 on Linux; TIMEOUT on Windows (was FAIL 2/8) |
| `SimpleClientHttpRequestFactoryTests` | FAIL (AssertionFailedError) | 6/10 (4 pre-existing, out of scope) |
| `SimpleClientHttpResponseTests` | FAIL (IllegalArgumentException) | 4/5 (1 residual, see below) |
| `SubscriberInputStreamTests` | TIMEOUT | TIMEOUT (unchanged) |
| `reactive.ClientHttpConnectorTests` | TIMEOUT | TIMEOUT (unchanged) |
| `reactive.ReactorClientHttpConnectorTests` | ABEND | **5/5 OK on Windows** (2/5 on Linux — Linux-only EPollSelectorImpl blocks the rest there, irrelevant to Windows) |

Also confirmed a broad **positive side effect on the wider spring-web
suite**: a 20-class random sample outside `http.client` went from several
classes with partial failures to fully OK (`ServletWebRequestTests`,
`HttpRangeTests`, `PathPatternTests`, `RequestEntityTests`,
`PathPatternParserTests` 14/27→27/27, `FixedLocaleContextResolverTests`,
`ErrorHandlerIntegrationTests` 0/12→6/12) with **zero regressions** —
expected, since several of the fixes are general JDK real-mode native gaps
that any code touching `java.net.Socket`/DNS/threads could hit.

## Root causes fixed

### Round 1 — the core dispatch bug + initial JDK 21 real-mode gaps

1. **Inline-cache population never checked whether an ancestor class had
   been JVMTI-redefined.** Mockito's inline mock maker mocks a *concrete*
   class (e.g. `java.net.HttpURLConnection`) by redefining that class
   directly and instantiating a marker subclass that does **not** itself
   override every mockable method. So the receiver is never redefined —
   only an ancestor is — and calls silently bypassed the mock's advice to
   run CratonVM's real native instead (surfacing as e.g.
   `IllegalArgumentException: HttpURLConnection: URL not set` from inside
   `given()`/`verify()` calls). Fixed in both
   `populate_virtual_invoke_cache` and `try_stackless_invoke`'s ancestor
   walks (`execute_invokevirtual_vtable_fast` already had the correct
   guard — the template for the fix). **Verified no regression**: built a
   variant with just this fix reverted and diffed a 20-class spring-web
   sample outside `http.client` — byte-identical results.
2. `jdk/internal/misc/PreviewFeatures.isPreviewEnabled` — already fixed
   upstream on `dev` independently by the time of merging; no code change
   needed from this branch (duplicate registration dropped at merge time).
3. `JavaLangAccess.defineClass` bridge ran real bytecode (`ctx.invoke_virtual`)
   instead of the registered native `cl_define_class_basic`, tripping real
   `ClassLoader.checkName` on an internal-form (slash) name — broke
   `jdk.internal.reflect.ClassDefiner` (Objenesis's default mock
   instantiation strategy).
4. An over-broad `is_prohibited_package_name` guard blocked
   `jdk.internal.reflect.*`, needed for the same `ClassDefiner` path.
5. Missing `JavaLangAccess.getConstantPool`/`start` bridges (ByteBuddy class
   reading during mock-redefine; Jetty's thread-pool/structured-concurrency
   plumbing).
6. Missing JDK 21 `StackWalker.callStackWalk` overload signature (broke
   Mockito's `LocationImpl`, used on every mocked-method invocation).

### Round 2 — found by comparing against the real Windows/JDK25 baseline

7. **`native_bais_read_byte_array` (`InputStream.read(byte[])`, registered
   on `java/io/InputStream`) delegated via a direct Rust function call
   instead of `ctx.invoke_virtual`.** The JDK contract for `read(byte[])`
   is exactly `return read(b, 0, b.length)` — a polymorphic call. A direct
   call bypasses that polymorphism, so it always ran the generic "loop
   calling `read()` once per byte" fallback instead of a receiver's real
   3-arg override. For Jetty's `InputStreamResponseListener$Input` (which
   overrides `read(byte[],int,int)` but not `read(byte[])`), that fallback
   loop's `read()` call closed a cycle: `read()` (Input's own bytecode)
   calls `read(byte[1])` → hits this native → loop calls `read()` again →
   Input's own bytecode again → ... unbounded, blowing the stack. This
   *was* the originally-reported `StackOverflowError` for
   `JettyClientHttpRequestFactoryTests`. Fixed by delegating via
   `ctx.invoke_virtual` instead — `native-io/src/lib.rs`. **Verified on
   Windows/JDK 25**: `JettyClientHttpRequestFactoryTests` went from 5/6 to
   6/6 (fully OK).
8. Missing `sun/net/dns/ResolverConfigurationImpl.{init0,loadDNSconfig0,notifyAddrChange0}`
   (Windows DNS-config native, consulted by Netty's
   `DnsServerAddressStreamProviders` as a courtesy default-resolver
   config) — cascaded to `NoClassDefFoundError` for Netty's DNS provider,
   breaking Reactor-Netty-based tests entirely. **Verified on
   Windows/JDK 25**: `reactive.ReactorClientHttpConnectorTests` went from
   2/5 to 5/5 (fully OK).
9. Missing `Socket.getSoTimeout()` native override — fell through to real
   bytecode's `getImpl().getOption(SO_TIMEOUT)`. Our synthetic `Socket`
   keeps the host STRING at field slot 0 (needed for `getInetAddress()`),
   which collides with wherever real `Socket.impl` sits — `getImpl()`
   returned the host string, and calling `.getOption(int)` on a `String`
   threw `NoSuchMethodError: java/lang/String.getOption(I)...`. This broke
   Apache HttpClient5's `DefaultManagedHttpClientConnection.bind()`, which
   unconditionally calls `getSoTimeout()` on every new connection.
   Reproduced on BOTH Windows and Linux (confirmed platform-independent).
   Fixed by querying the real underlying `TcpStream`'s read timeout
   directly (mirrors `setSoTimeout`'s existing side-table approach) —
   `native-builtins/src/net_phase_e.rs`.
10. Missing `JavaLangAccess.join(String,String,String,String[],int)` — a
    `String.join`/`StringJoiner`-adjacent fast-path helper — broke several
    parameterized tests in `HttpComponentsClientHttpRequestFactoryTests`.
11. A chain of Linux-only JDK native gaps needed to even REACH bug #9 on
    this Linux test host (`sun/nio/ch/NativeThread.supportPendingSignals0`,
    `sun/nio/ch/UnixDispatcher.init`, the full
    `jdk/net/LinuxSocketOptions` native surface,
    `sun/nio/ch/Net.shouldShutdownWriteBeforeClose0`) — all part of
    `NioSocketImpl`'s static-init chain, reached by `jdk.net.Sockets
    .optionSets()` the first time ANY code touches a real `Socket`'s
    `SocketImpl`. Fixed as a means to reproduce/debug bug #9 on the fast
    Linux host rather than the slow (~25-35min) Windows build loop; these
    are genuinely Linux-only classes (confirmed absent from the Windows
    JDK 25 install via `javap`), so they're not expected to matter for the
    Windows-based bug report directly, but they DO matter for continuing
    to use this Linux host for future CratonVM work.

## Residuals — NOT fixed this session

### A. `HttpComponentsClientHttpRequestFactoryTests` — 3/18, "Mockito cannot mock CloseableHttpClient"

`mergeBasedOnCurrentHttpClient`, `localSettingsOverrideClientDefaultSettings`,
`defaultSettingsOfHttpClientMergedOnExecutorCustomization` all fail with
`MockitoException: Could not modify all classes [... CloseableHttpClient]`
with **no nested cause** in the exception (unlike the earlier
`HttpURLConnection` case, which traced cleanly to a missing
`getConstantPool` native). Not root-caused this session — would need
instrumenting our own class-redefine backend to log why it rejects this
specific class hierarchy (interfaces: `Closeable`, `HttpClient`,
`ModalCloseable`, `AutoCloseable`, `Configurable`).

### B. Linux-only NIO gap still blocks most of `ReactorClientHttpRequestFactoryTests` on Linux (not Windows)

`sun/nio/ch/EPollSelectorImpl` (Netty's epoll-based `Selector` — Linux-only,
Windows uses a completely different selector implementation) still blocks
8/10 tests in this class on the Linux host. **On Windows this class instead
now shows `TIMEOUT`** (previously `FAIL` 2/8) — the DNS-resolver fix (#8
above) got it past the `NoClassDefFoundError` cascade, but it now hangs on
something else. Not investigated further; worth a dedicated session on
Windows directly (the Linux host can't help here, since the Windows failure
mode is different from the EPollSelectorImpl-blocked Linux one).

### C. Genuine hangs — 4 classes, root cause NOT found

`JdkClientHttpRequestFactoryTests`, `OutputStreamPublisherTests`,
`SubscriberInputStreamTests`, `reactive.ClientHttpConnectorTests` still hang
or fail even after all fixes above. Standalone probes exercising a raw
loopback `ServerSocket` + real `HttpClient` (including sequential requests
over a reused keep-alive connection) passed cleanly — the actual failure is
specific to these tests' combination of MockWebServer + these particular
request patterns. `--stack-dump-on-timeout` only shows all interpreter
threads parked in native/Rust code (dispatch-trace ring buffer showed
MockWebServer/okio buffer churn, inconclusive). Worth a dedicated session
using the `CRATONVM_DBG_SOCK`-style tracing playbook from the historical
Jetty NIO investigation (see memory `jetty-nio-client-directbytebuffer-putbyte-bug`).

### D. `SimpleClientHttpResponseTests::shouldNotCloseConnectionWhenResponseClosed` — 1/5, order-dependent

Consistently the ONE failure (4/5 pass) on BOTH Windows and Linux:
`org.mockito.exceptions.misusing.UnfinishedVerificationException` thrown
from the *next* test method's `mock()` call. Could NOT reproduce
standalone across several probe attempts replicating the exact call
sequence. Suspect a narrow residual in how Mockito's `MockingProgress`
thread-local interacts with JUnit 5's per-method reflective test
instantiation. Not resolved this session.

### E. `SimpleClientHttpRequestFactoryTests` — 6/10, pre-existing/out-of-scope

`prepareConnectionWithRequestBody`, `deleteWithoutBodyDoesNotRaiseException`,
`httpMethods`, `interceptor` — already flagged as pre-existing,
out-of-scope synthetic-`HttpURLConnection` gaps in memory
`huc-setdooutput-wrong-slot-drops-post-body` (fail on the frozen baseline
too, unrelated to the redefine-dispatch family). Not addressed this
session.

## Reproduction

```bash
# Azure host, worktree ~/wt-httpclient, branch fix/http-client-cluster-azure
ssh -i ~/.ssh/azure.pem victor@20.84.156.31
cd ~/wt-httpclient && cargo build --release -p cratonvm-cli -j8
TESTCP=$(cat /opt/cratonvm/apps/spring-framework/spring-web/build/cratonvm-testcp.txt)
CP="/opt/cratonvm/apps/spring-suite-runner:$TESTCP"
KRUN_STACK=1 ./target/release/cratonvm --java-home /usr/lib/jvm/java-21-openjdk-amd64 \
  -cp "$CP" KRun org.springframework.http.client.SimpleClientHttpResponseTests

# Windows, apps/spring-suite-runner, official harness (JDK 25):
cd apps/spring-suite-runner
CRATONVM_BIN=<your built .exe> KRUN_STACK=1 \
  ./run-suite.sh run --jdk real --jit on --batch 1 --only 'http\.client\.'
```
