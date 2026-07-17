# Fixed: wrong-receiver virtual dispatch: `String.setOption`/`File.get()` NoSuchMethodError cluster

**Status: FIXED — 2026-07-17**

## Resolution

- **Case 1:** real-network mode now excludes only legacy
  `SSLSocketFactory` `SyntheticStub` registrations. The later P68
  `Bridge` registrations remain live, perform the TLS work, and create
  layout-correct `SSLSocket` objects. This prevents a two-slot synthetic
  `Socket` from reaching real `Socket` bytecode while avoiding the
  unsupported fallback to the full JDK JSSE implementation.
- **Case 2:** JUL's native parameter renderer was invoking `get()` on every
  non-String argument, treating ordinary objects such as `File` as
  `Supplier`s. It now calls `get()` only for actual
  `java.util.function.Supplier` instances and otherwise uses `toString()`.

Regression coverage runs the real-network loopback probe and asserts that
the `SSLSocketFactory` registry contains bridge methods and no synthetic
stubs. A second fixture drives `Logger.log(Level, String, Object[])` with a
`File` argument in both interpreter and JIT modes. Both pass on the remote
build host. The remainder of this document is retained as the original
investigation record.

## Symptom — Case 1: `java/lang/String.setOption(ILjava/lang/Object;)V`

9 classes across the 2026-07-16 rerun fail identically. Full log
(`core/spring-boot-test`):

```
=> java.lang.NoSuchMethodError: java/lang/String.setOption(ILjava/lang/Object;)V
   java.net.Socket.setSoLinger(Socket.java:1164)
   org.apache.http.impl.conn.BHttpConnectionBase.shutdown(BHttpConnectionBase.java:304)
   org.apache.http.impl.conn.DefaultManagedHttpClientConnection.shutdown(DefaultManagedHttpClientConnection.java:95)
   org.apache.http.impl.conn.LoggingManagedHttpClientConnection.shutdown(LoggingManagedHttpClientConnection.java:98)
   org.apache.http.impl.conn.PoolingHttpClientConnectionManager$2.process(PoolingHttpClientConnectionManager.java:420)
   org.apache.http.pool.AbstractConnPool.enumLeased(AbstractConnPool.java:597)
   org.apache.http.impl.conn.CPool.enumLeased(CPool.java:81)
   org.apache.http.impl.conn.PoolingHttpClientConnectionManager.shutdown(PoolingHttpClientConnectionManager.java:413)
   org.apache.http.impl.execchain.MainClientExec.execute(MainClientExec.java:368)
   ... (RetryExec, ServiceUnavailableRetryExec, RedirectExec, InternalHttpClient, CloseableHttpClient)
   org.eclipse.aether.transport.http.HttpTransporter.execute(HttpTransporter.java:500)
   org.eclipse.aether.transport.http.HttpTransporter.implGet(HttpTransporter.java:450)
   org.eclipse.aether.spi.connector.transport.AbstractTransporter.get(AbstractTransporter.java:64)
   org.eclipse.aether.connector.basic.BasicRepositoryConnector$GetTaskRunner.runTask(BasicRepositoryConnector.java:482)
   ... (Aether dependency-collection stack down to DefaultArtifactResolver)
   org.springframework.boot.testsupport.classpath.ModifiedClassPathClassLoader.resolveCoordinates(ModifiedClassPathClassLoader.java:258)
   org.springframework.boot.testsupport.classpath.ModifiedClassPathClassLoader.getAdditionalUrls(ModifiedClassPathClassLoader.java:237)
   org.springframework.boot.testsupport.classpath.ModifiedClassPathClassLoader.processUrls(ModifiedClassPathClassLoader.java:222)
   org.springframework.boot.testsupport.classpath.ModifiedClassPathClassLoader.compute(ModifiedClassPathClassLoader.java:142)
   org.springframework.boot.testsupport.classpath.ModifiedClassPathClassLoader.get(ModifiedClassPathClassLoader.java:114)
   org.springframework.boot.testsupport.classpath.ModifiedClassPathExtension.interceptMethod(ModifiedClassPathExtension.java:93)
```

Full log: `apps/spring-boot-suite-runner/.suite/results/rerun-20260716/shard2/logs/core_spring-boot-test.org.springframework.boot.test.json.DuplicateJsonObjectContextCustomizerFactoryTests.out.log`

Confirmed present, same signature/same call site (`Socket.setSoLinger` →
`getImpl().setOption(...)`), in all of:

- `core/spring-boot`: `Log4J2LoggingSystemTests`, `SpringProfileArbiterTests`
- `core/spring-boot-autoconfigure`: `ConditionalOnCheckpointRestoreTests`
- `core/spring-boot-test`: `DuplicateJsonObjectContextCustomizerFactoryTests`
- `module/spring-boot-flyway`: `Flyway110AutoConfigurationTests`
- `module/spring-boot-gson`: `Gson210AutoConfigurationTests`
- `module/spring-boot-liquibase`: `Liquibase423AutoConfigurationTests`
- `test-support/spring-boot-test-support`:
  `ModifiedClassPathExtensionOverridesParameterizedTests`,
  `ModifiedClassPathExtensionOverridesTests`

All 9 go through the **identical** trigger: `ModifiedClassPathClassLoader`'s
Aether/Eclipse-Maven-Resolver artifact download (`resolveCoordinates` →
`DefaultArtifactResolver` → Apache HttpClient) shutting down a pooled HTTP
connection at the end of the request, which calls `Socket.setSoLinger()` →
real `Socket.getImpl()` → `SocketImpl.setOption(int, Object)`. **This is one
shared trigger path, not 9 independent occurrences.**

## Root cause — Case 1 (CONFIRMED at file:line precision)

This is the same bug *family* previously found and partially fixed on
`dev` in the DoHead investigation
(`docs/internal/fixed-suite-bugs/dohead-jit-heap-corruption-register-invisibility-FIXED.md`),
just through a **producer the earlier fix didn't cover**.

### The mechanism (established, not new)

`CRATONVM_REAL_NET_SOCKETS=1` (set by both the Tomcat and Spring Boot suite
runners — `apps/spring-boot-suite-runner/run-spring-boot-suite.ps1:693`)
drops every native registered on `java/net/Socket` at
`native-api/src/registry.rs:3535-3563`, so **real** `java.net.Socket`
bytecode runs (`getImpl()`, `setSoLinger()`, etc. all execute the genuine
JDK `.java` source, with the genuine real-class field layout: `impl`,
`socketLock`, `closeLock`, `created`, `bound`, `connected`, `closed`, …).

That is only safe if the `Socket` **object itself** was also constructed
through real bytecode (real `<init>`, which sets all those fields to sane
initial values). If some *other* native path fabricates a `java/net/Socket`
object directly via `alloc_concurrent_synthetic` with fewer slots than the
real class declares, and hands that object to code that then runs the real
`getImpl()`/`setSoLinger()` bytecode, the real-field read lands on
whatever garbage occupies that slot in the undersized/wrong-shaped object —
producing exactly this shape of bug: a `NoSuchMethodError` naming the class
of *whatever happened to be in that slot* (here, a leftover `String`, e.g. a
host name) as the receiver of a completely unrelated method
(`setOption`) that real code never intended to call on a `String`.

This exact mechanism, for the exact same call site
(`Socket.setSoTimeout`/`setSoLinger` → `getImpl().setOption`), was already
found and fixed **twice** for two other producers:

1. `javax/net/SocketFactory.createSocket()` (fixed commit `bd03eb243`,
   confirmed ancestor of this worktree's HEAD) — extended the RNS
   drop-filter (`native-api/src/registry.rs:3535-3563`) to also cover
   `class_name == "javax/net/SocketFactory"`, so real
   `SocketFactory`/`DefaultSocketFactory` bytecode constructs sockets via
   the real `Socket` constructor instead of the phase52 5-slot synthetic
   allocator (`native-builtins/src/phases_early.rs:10675`,
   `phase52_alloc_socket`).
2. Bare-allocated `java/net/Socket` objects missing `socketLock`/`impl`
   seeding (fixed commit `c0a0450ef`).

### The gap (NOT fixed): `javax/net/ssl/SSLSocketFactory`

`native-builtins/src/tls.rs:1568-1615`, `register_ssl_socket_factory`,
registers (tagged `NativeKind::SyntheticStub`):

```rust
// createSocket(String host, int port) -> Socket
r.register(cls, "createSocket", "(Ljava/lang/String;I)Ljava/net/Socket;", |ctx, _args| {
    let sock = alloc_concurrent_synthetic(ctx, "java/net/Socket", 2);   // <-- 2 slots only
    let sock = crate::net_phase_e::re1_init_socket_locks(ctx, sock);
    Ok(Some(Value::Object(Some(sock))))
});

// createSocket(Socket s, String host, int port, boolean autoClose) -> Socket
r.register(cls, "createSocket", "(Ljava/net/Socket;Ljava/lang/String;IZ)Ljava/net/Socket;", |ctx, _args| {
    let sock = alloc_concurrent_synthetic(ctx, "java/net/Socket", 2);   // <-- same
    let sock = crate::net_phase_e::re1_init_socket_locks(ctx, sock);
    Ok(Some(Value::Object(Some(sock))))
});
```

This is the **layered-socket** overload Apache HttpClient's
`SSLConnectionSocketFactory` uses to upgrade a plain `Socket` to TLS
(`sslSocketFactory.createSocket(plainSocket, host, port, true)`), and the
plain `createSocket(String,int)` overload used elsewhere. It hands back a
bare **2-field** synthetic `java/net/Socket` — even fewer fields than the
already-buggy phase52 5-field allocator that motivated fix #1 above.

Critically, `native-api/src/registry.rs`'s RNS drop-filter
(`real_net_sockets_enabled() && (class_name == "java/net/Socket" ||
class_name == "java/net/ServerSocket" || class_name ==
"javax/net/SocketFactory")`, lines 3535-3563) checks the class name by
**exact string equality** and lists `javax/net/SocketFactory` but **not**
`javax/net/ssl/SSLSocketFactory`. It also isn't caught by the
`drop_synthetic_stubs`/`CRATONVM_NO_STUBS` mechanism (`registry.rs:3511`,
`3419`), because the Spring Boot suite runner does not set
`CRATONVM_NO_STUBS` (confirmed: no such variable appears anywhere in
`run-spring-boot-suite.ps1`).

So under the suite runner's actual environment
(`CRATONVM_REAL_NET_SOCKETS=1`, `CRATONVM_NO_STUBS` unset):
- Real `java/net/Socket` bytecode runs for every Socket method (via the RNS
  filter dropping Socket's own natives) — **including** `setSoLinger`,
  `getImpl()`, `shutdown()`.
- `javax/net/ssl/SSLSocketFactory.createSocket(...)` STILL returns the
  2-field synthetic object from `tls.rs`, because that class name is not in
  the RNS filter and `drop_synthetic_stubs` is off.
- Apache HttpClient's HTTPS connection pool (used by Aether to talk to the
  Maven repository over HTTPS during `ModifiedClassPathClassLoader`'s
  artifact resolution) obtains its socket via exactly this call, then later
  calls real `Socket.setSoLinger()`/`shutdown()` bytecode on it at
  connection-pool teardown.
- Real `getImpl()` reads the real `impl` field slot on an object that was
  never laid out with that field — landing on whatever adjacent data
  `alloc_concurrent_synthetic`'s 2-slot allocation/`re1_init_socket_locks`
  left there (a leftover `String`, per the analogous, already-documented
  phase52 case: "the host String lands on `impl`" —
  `native-api/src/registry.rs:3547-3548`) — and dispatches
  `setOption(int,Object)` on it, producing `NoSuchMethodError:
  java/lang/String.setOption(ILjava/lang/Object;)V`.

There are several other `javax/net/ssl/SSLSocketFactory` registration
functions in this codebase with the same shape
(`native-builtins/src/net_phase_e.rs:8514,8698,8910`,
`native-builtins/src/t27_tls.rs:2832,2875`,
`native-builtins/src/phases_late.rs:42520,42575,70567,70574`) — whichever
one actually wins registration for the live build should be checked too,
but `tls.rs`'s `register_ssl_socket_factory` (called from the TLS
registration pass, `tls.rs:2474`) is a confirmed, concrete instance of the
gap and is sufficient to explain the symptom.

**This is a regression/gap in fix #1 above, not a new defect class**: the
2026-07-13 fix correctly identified and closed the plain-`SocketFactory`
instance of "synthetic-Socket producer + real-bytecode consumer under RNS"
but did not audit/extend to the TLS sibling, which has the identical shape
and is the one actually exercised by any HTTPS-based artifact fetch (which
`ModifiedClassPathClassLoader`'s Aether path is, hitting a Maven Central
mirror over HTTPS).

**Why this wasn't caught by Tomcat's DoHead validation:** the DoHead family
validated the Tomcat suite (server-side sockets, `ServerSocket`/plain client
sockets in `Http2TestBase`), which doesn't exercise
`SSLSocketFactory.createSocket()`'s **layered-socket** overload the way an
outbound HTTPS client connection (Aether → Maven Central) does. This is the
first suite/workload in this investigation trail to exercise that producer
under `CRATONVM_REAL_NET_SOCKETS=1`.

### Is this JIT or interpreter?

Neither, for Case 1. This is a **construction-time object-layout defect** —
the wrong-looking receiver is a real, correctly-typed `String` object that
was actually stored at that slot; the interpreter/JIT dispatch code is
behaving correctly given a malformed input object. `--nojit` would not be
expected to change this (not tested here, but the mechanism has zero
dependency on JIT-compiled code — it's synthetic-native object
construction, independent of whether the *methods later called on it* are
interpreted or JIT'd). This is **not** a fresh regression from the
same-day interpreter/JIT merge (`vm/src/runtime/interpreter.rs`,
`jit/src/x64.rs`) described in the investigation brief — the relevant code
(`tls.rs:1568-1615`, the RNS filter in `registry.rs`) is unrelated to and
untouched by that merge; this is a **pre-existing gap** in the 2026-07-13
SocketFactory fix that a different call path (HTTPS Aether artifact fetch)
newly exercises in this rerun.

## Symptom — Case 2: `java/io/File.get()Ljava/lang/Object;`

`module/spring-boot-jetty` `SslServerCustomizerTests` CRASHES:

```
NoSuchMethodError method="java/io/File.get()Ljava/lang/Object;" caller="org/conscrypt/NativeLibraryLoader.log(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V @pc=22"
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/rerun-20260716/shard7/logs/module_spring-boot-jetty.org.springframework.boot.jetty.SslServerCustomizerTests.err.log`

The same log also shows, just before the crash, a burst of unrelated
`gen_heap::get_field: out-of-bounds field read dropped` WARN lines against
`org/junit/jupiter/engine/execution/InterceptingExecutableInvoker` and
`InvocationInterceptorChain` (`num_slots=0`) — **this is pre-existing,
tracked noise** (see
`docs/internal/app-jvm-bugs/bug-wildfly-get-field-factory-noise.md`,
same guard/same message, different classes), not part of this crash's
causal chain; it is a red herring and should not be conflated with the
`File.get()` NoSuchMethodError.

## Analysis — Case 2 (NOT confirmed to share Case 1's mechanism)

**Do not assume this is the same bug as Case 1.** The evidence gathered in
this pass does not establish that. Differences:

- The receiver class involved (`java/io/File`) has its own synthetic
  allocation path (`native-builtins/src/phases_late.rs:15834`,
  `file_alloc`, a single-slot `path` synthetic used uniformly by
  `register_phase57_file`'s `File` natives), but — unlike `Socket` — there
  is **no `CRATONVM_REAL_NET_SOCKETS`-style env-gated split** that makes
  some `File` methods run real bytecode while others run natively on a
  differently-shaped object. `File` natives are Bridge-category and used
  consistently, so the "producer/consumer layout split-brain" mechanism
  that explains Case 1 does not have an obvious analogue for `File`.
- `NativeLibraryLoader.log(String, Object, Object)` is Conscrypt library
  code (a third-party JAR on the test classpath), not CratonVM-registered
  native code — no CratonVM-side native override for this method or class
  was found in `native-builtins/src`. Whatever is happening here is
  happening purely in bytecode dispatch (interpreted or JIT'd), not in a
  native-construction gap.
- A `.get()` call at bytecode offset 22 inside a logging helper strongly
  suggests the real source does something like unwrap an `Optional`/
  `AtomicReference`/similar `.get()`-bearing type among its logged
  arguments, and the **receiver operand** for that `invokevirtual`/
  `invokeinterface` ended up being a different local (a `File`-typed
  argument passed to `log(...)`) than intended. That is consistent with
  the "wrong local read as receiver" hypothesis from the investigation
  brief, but this pass did not find a concrete file:line defect in
  `execute_invokevirtual_cached`/`execute_invokevirtual_vtable_fast`
  (`vm/src/runtime/interpreter.rs`) that would explain it — both of those
  functions **do** validate `actual_class_id == receiver_class_id` (or
  recompute the receiver fresh from the operand stack in the vtable-fast
  path) before dispatching a cached target
  (`interpreter.rs:33417-33419`, `:33710-33712`, `:32662-32667`), which
  rules out the most obvious "stale cached target served for a new
  receiver" shape of bug for the *cached* dispatch path specifically.
- The recently-landed `4290124b4` ("perf(vm): consult monomorphic invoke
  cache on the JDK-class slow path") changed `invokevirtual`/
  `invokespecial`/`invokeinterface` dispatch to try
  `execute_invokevirtual_cached` before falling back to the historical
  slow path (`vm/src/runtime/interpreter.rs`, the `Instruction::Invokevirtual`/
  `Invokeinterface` arms, ~line 13927-14015). This is exactly the kind of
  same-day dispatch-path change the investigation brief warned about, and
  it is the most likely place a wrong-receiver bug of this shape would
  live if one exists — but this pass could not find a concrete defect in
  it beyond noting it as the most probable *location* to keep
  investigating (its receiver-identity checks all read correct as
  written).

**Conclusion for Case 2**: real, reproducible `NoSuchMethodError` with the
same "wrong receiver" *shape* as Case 1, but the causal chain is NOT
established. Filed here for tracking and cross-reference, not as a
confirmed instance of Case 1's root cause. Whoever picks this up next
should get a symbolized native crash dump / `CRATONVM_DBG_INVOKESTATS`-style
trace of the actual bytecode at `NativeLibraryLoader.log` pc≈22 (decompiled
Conscrypt source or a `javap -c` dump of the actual jar on the classpath)
before further hypothesizing.

## Repro

```powershell
cd apps\spring-boot-suite-runner

# Case 1 — any of the 9 String.setOption classes, e.g.:
.\run-spring-boot-suite.ps1 -Category all -Start 1 -Count 0 `
  -ClassList <TSV row for core/spring-boot-test, DuplicateJsonObjectContextCustomizerFactoryTests> `
  -Exe C:\craton\CratonVM-spring-boot-crashfail-20260714\target\release\cratonvm-spring-boot-rerun-20260716.exe

# Case 2:
.\run-spring-boot-suite.ps1 -Category all -Start 1 -Count 0 `
  -ClassList <TSV row for module/spring-boot-jetty, SslServerCustomizerTests> `
  -Exe C:\craton\CratonVM-spring-boot-crashfail-20260714\target\release\cratonvm-spring-boot-rerun-20260716.exe
```

(Or re-run the existing `rerun-20260716` shard2/shard7 result sets directly —
logs already captured at the paths cited above.)

## What to fix (Case 1)

Extend the RNS drop-filter in `native-api/src/registry.rs:3535-3563` to also
cover `class_name == "javax/net/ssl/SSLSocketFactory"` (mirroring the
`javax/net/SocketFactory` fix, commit `bd03eb243`), so real
`SSLSocketFactory`/`SSLSocketFactoryImpl` bytecode constructs its layered
socket via the real `Socket` constructor path instead of
`tls.rs`'s 2-slot `alloc_concurrent_synthetic` allocator. Audit the other
`javax/net/ssl/SSLSocketFactory` registration sites listed above
(`net_phase_e.rs`, `t27_tls.rs`, `phases_late.rs`) for the same gap — only
one of them is live in a given build, but whichever one is should get the
same treatment (or the filter should short-circuit all of them uniformly
regardless of which one wins registration, since the filter operates at
`register()` time before any of them run).

## Related

- `docs/internal/fixed-suite-bugs/dohead-jit-heap-corruption-register-invisibility-FIXED.md` —
  the original `String.setOption` diagnosis and the `javax/net/SocketFactory`
  fix (commit `bd03eb243`) this bug is a gap in.
- `native-api/src/registry.rs:3526-3563` — the RNS drop-filter that needs
  extending.
- `native-builtins/src/tls.rs:1568-1615` (`register_ssl_socket_factory`) —
  the confirmed producer of the undersized synthetic `Socket`.
- `native-builtins/src/phases_early.rs:10675` (`phase52_alloc_socket`) — the
  sibling 5-field allocator already fixed for the plain-`SocketFactory` case;
  same "host String lands on `impl`" mechanism, smaller field count here.
- `docs/internal/app-jvm-bugs/bug-wildfly-get-field-factory-noise.md` — the
  unrelated `num_slots=0` noise seen in the Case 2 log, not part of its
  causal chain.
- `vm/src/runtime/interpreter.rs`, `execute_invokevirtual_cached` /
  `execute_invokevirtual_vtable_fast` — reviewed for Case 2, receiver-identity
  checks look correct; flagged as the most likely location for further
  investigation, not a confirmed defect site.
