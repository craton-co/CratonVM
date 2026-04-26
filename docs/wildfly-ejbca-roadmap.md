# RustJVM — JDK 25 compat roadmap (any Java app)

**Re-scoped 2026-04-24 (session 93).** Original wildfly+ejbca framing rejected (`memory/feedback_wildfly_is_jdk_compat.md`): WildFly/EJBCA/Keycloak/Quarkus/Tomcat are *forcing-function fixtures*, not scope drivers. The work is closing OpenJDK 25 surface gaps — once those close, every Java JAR that runs on stock HotSpot 25 runs on rust-jvm unchanged.

This doc is organized as **waves of parallel work packages (WPs)**. Waves are sequential (Wave N depends on Wave N-1 core deliverables). WPs within a wave are pairwise file-disjoint and designed to be dispatched as concurrent agents.

> Session 92 baseline: Keycloak bootstrap alive at 60s with only the 9 expected `MISSING:` natives, all closed in WP0.3. CHM `initTable` livelock fixed. vm synthetic-jdk 3253/0.
>
> Session 93 baseline: WP0.1 println P0 fix landed; OSC scaffolding landed; bench/wildfly EJBCA smoke harness pinned. Wave 1 partially landed (rate-limit cut).

---

## 0. TL;DR

| Wave | Theme | # WPs | Parallel agents | Blocks |
|---|---|---|---|---|
| 0 | Emergency P0 fixes | 5 | 5 | All downstream |
| 1 | Boot infra (JDK boot-time gaps) | 12 | 6 | Waves 2-8 |
| 2 | Reflection + runtime bytecode gen | 10 | 5 | Waves 4-7 |
| 3 | NIO / async I/O | 8 | 5 | Wave 5 |
| 4 | Concurrency primitives | 8 | 4 | All later |
| 5 | Net + TLS | 9 | 5 | Wave 6 |
| 6 | JCE / crypto provider chain | 8 | 5 | — |
| 7 | JDBC + ServiceLoader-based SPI | 3 | 2 | — |
| 8 | Stabilization, perf, CI | 8 | 5 | — |
| **Total** | | **≈ 71** | peak ≈ 6 agents/wave | |

Rough budget: ~71 WPs × 0.5-4 dev-days avg ≈ 100-200 focused dev-days. With 4-6 parallel agents per wave and the same cadence as T17/T18, a 3-5 calendar-month push is realistic.

---

## 1. Success criteria (definition of done)

- [ ] **S0**: Stock OpenJDK 25 spec surface complete — no `todo!()` / `unimplemented!()` / `panic!("not yet"|"todo"|"stub")` in `native-*/src/**`, `vm/src/runtime/**` outside `#[cfg(test)]`. Tracked in `docs/stub-census.md`.
- [ ] **S1**: any pure-Java JAR (no JNI) that runs on HotSpot 25 with `java -jar foo.jar` runs on rust-jvm with `rustjvm -jar foo.jar`. No JVM-side patches required.
- [ ] **S2**: 10 forcing-function apps boot to first user interaction. Suggested set: Keycloak 16/26, EJBCA CE 9, Tomcat 10, Jetty 12, Quarkus 3 (dev mode), Spring Boot 3 (Petclinic), Maven 3.9, Gradle 8 (daemon), Apache Kafka 3 (broker), Apache Cassandra 5. Each gets a smoke fixture under `bench/<app>/`.
- [ ] **S3**: bench-hotspot-compare geomean ≤ 2× HotSpot 25 on representative workloads (DaCapo + Renaissance subset).
- [ ] **S4**: 24h soak with one of the S2 apps under a synthetic load — heap growth ≤ 2× initial, no FD/native-mem leaks, GC pause p99 < 50ms, clean exit.
- [ ] **S5**: TCK-equivalent test harness ≥90% pass on the OpenJDK 25 jdk_lang/jdk_util/jdk_io/jdk_net/jdk_security/jdk_concurrent suites (best-effort — full TCK requires certification we don't have).

---

## 2. Architectural map — JDK 25 features rust-jvm must close

| JVM feature | Used by | rust-jvm status (session 93) |
|---|---|---|
| `sun.misc.Unsafe.compareAndSet*` | ConcurrentHashMap, AQS | ✅ fixed session 92 |
| `sun.misc.Unsafe.defineClass` / `Lookup.defineClass` | ByteBuddy, CGLIB, Hibernate, Weld proxies | ❌ stub |
| `java.lang.invoke` (MH, VH, LambdaMetafactory, StringConcatFactory) | Java 11+ idioms everywhere | ⚠️ partial |
| `java.lang.reflect` full coverage | Annotation scanners, DI containers | ⚠️ partial |
| `java.io.ObjectStreamClass` / `Serializable` fastpath | RMI, JUnit4 `Result`, distributed cache | ⚠️ basics landed (WP0.2) |
| `java.nio.channels.Selector` epoll / IOCP | Netty, Undertow, Jetty | ⚠️ partial |
| `FileChannel.map` mmap | H2 page store, indexed files | ❌ stub |
| `java.util.concurrent.*` (AQS, CF, FJP) | Everything | ⚠️ partial |
| `sun.security.ssl.*` internal TLS state | HTTPS server + client | ⚠️ partial |
| `javax.net.ssl.SSLEngine` | Netty, Undertow, async TLS | ⚠️ partial |
| JCE provider chain + BouncyCastle | Crypto-heavy apps (CAs, signers) | ⚠️ partial |
| `java.lang.instrument` | Debug agents, Mockito MockMaker, Jacoco | ❌ stub |
| `jdk.internal.*` (SharedSecrets, BootLoader, VM) | JDK internals | ⚠️ partial |
| `java.security.Policy` real parser | Apps with `-Djava.security.manager` | ✅ session 86 |
| Runtime proxy generation (`java.lang.reflect.Proxy`) | JAX-RS client, JDK service factories | ⚠️ partial |
| Annotation retention RUNTIME parsing | DI containers, JPA discovery | ⚠️ partial (WP1.7 landed) |
| `java.util.ServiceLoader` | JDBC drivers, JAX-RS providers | ⚠️ classpath scan needs WP1.8 fix |
| `java.lang.StackWalker` | Logging frameworks, exception filtering | ⚠️ partial (WP1.9 stub-residual) |
| `java.lang.ref.Cleaner` / `PhantomReference` | DirectByteBuffer cleanup | ⚠️ phantom OK; cleaner partial |
| `java.lang.System.getenv` / `getProperties` | Every app's config | ⚠️ partial |
| `java.lang.ProcessBuilder` | Build tools, IDE integrations | ❌ stub |

Legend: ✅ done / ⚠️ partial / ❌ stub or broken.

---

## 3. Wave 0 — Emergency P0 fixes (block everything)

Dispatch: **5 parallel agents**, all WPs pairwise file-disjoint.

### WP0.1 — `System.out`/`System.err` double-print NPE  [S, 0.5d]  ✅ DONE
- **Outcome**: `TwoPrint` (2+ consecutive println on out+err) runs to completion, RC=0.
- **Resolution (session 93)**: `vm/src/vm/vm_init.rs:1985-1990` — `PrintStream` heap-alloc was hard-coded to 1 slot; real-JDK `PrintStream` has ~20 fields, so subsequent `getfield out` read OOB. Fix: `num_total_fields.max(1)`. Regression test `vm/tests/wp0_1_println_regression.rs`.

### WP0.2 — `ObjectStreamClass.serializableConstructor` null  [M, 1d]  ⚠️ basics landed
- **Outcome**: JUnit4 `Result.<clinit>` and any `Serializable` class statically initializes without NPE.
- **Status (session 93)**: scaffolding at `vm/src/runtime/serialization/{mod.rs,oscache.rs}` + extension of `native-builtins/src/phases_late.rs::objectstreamclass_natives`. **Open**: `apps/serializable_smoke` runtime-errors with "expected long on stack, got null" — operand-stack tag mismatch on OSC-native return path. Fix family same as session-91 K1/K2.
- **Acceptance**: round-trip `ArrayList`, `HashMap`, `CopyOnWriteArraySet`, `AtomicReference` byte-compatible with HotSpot 25.

### WP0.3 — close last `MISSING:` natives  [M, 1-2d]  ✅ DONE
- **Outcome**: 0 `MISSING:` natives during enterprise-app boot.
- **Resolution (session 92+93)**: 191/191 native coverage. Maps `docs/kc{16,26}-blocker-map.md` flipped to RESOLVED.

### WP0.4 — stub census  [S, 0.5d]  ✅ DONE
- **Outcome**: every `todo!()` / `unimplemented!()` / `panic!("not yet"|"todo"|"stub"` in `native-*` and `vm/src/runtime` listed with owner WP and "reachable by app boot? Y/N".
- **Resolution (session 93)**: `docs/stub-census.md` refreshed — 34 real stubs. CI no-stubs gate at `vm/src/runtime/interpreter.rs:10983-11010` already enforces zero in production paths.

### WP0.5 — generic enterprise-app smoke harness  [S, 0.5d]  ✅ DONE
- **Outcome**: a CI-bound script that pins today's reality as a repeatable failure set so regressions are detected.
- **Resolution (session 93)**: `bench/wildfly/` + `.github/workflows/ejbca-smoke.yml`. Schema v1 baseline pins WP0.1 NPE so the diff fires after any fix.
- **Note**: extend to other S2 apps in Wave 8.

---

## 4. Wave 1 — Boot infra for JDK 11+ idioms

Prereq: Wave 0 green. Dispatch: **6 parallel agents** (12 WPs across 6 owners, pair-bundled).

### WP1.1 — `java.io.ObjectStreamClass` complete  [L, 3-4d]  *(owner A)*
- **Outcome**: every `Serializable` class gets a real `ObjectStreamClass` with `serializableConstructor`, `writeObjectMethod`, `readObjectMethod`, `readResolveMethod`, `writeReplaceMethod`, `fields[]` in declaration order.
- **Files**: `vm/src/runtime/serialization/`, `native-builtins/src/phases_late.rs`.
- **Acceptance**: round-trip serialization of `ArrayList<String>`, `HashMap<String,Integer>`, `CopyOnWriteArraySet`, `AtomicReference` — all byte-compatible with HotSpot 25.

### WP1.2 — `sun.misc.Unsafe` full coverage  [L, 3d]  *(owner A)*
- **Outcome**: every `@HotSpotIntrinsicCandidate` method on `jdk.internal.misc.Unsafe` + legacy `sun.misc.Unsafe` has a real impl.
- **Files**: `native-builtins/src/unsafe_natives.rs`, `vm/src/runtime/unsafe_helpers.rs`.
- **Minimum list**: `objectFieldOffset(Field)`, `objectFieldOffset(Class,String)`, `staticFieldOffset/Base`, `allocateInstance`, `compareAndSet{Int,Long,Reference,Object}`, `weakCompareAndSet*`, `get/putOpaque`, `get/putAcquire`, `get/putRelease`, `fullFence`, `loadFence`, `storeFence`, `storeStoreFence`, `park/unpark/park(blocker,nanos)`, `invokeCleaner`, `copyMemory`, `setMemory`, `allocateMemory`, `reallocateMemory`, `freeMemory`, raw-pointer get/put, `defineClass(name,b,off,len,loader,pd)`, `defineAnonymousClass` (JDK 8 legacy), `throwException`, `getLoadAverage`.
- **Acceptance**: `apps/unsafe_probe/` runs 50+ assertions covering each op; CHM `size()` accurate under 16-thread contention.

### WP1.3 — `jdk.internal.misc.VM` accurate init-level  [M, 1-2d]  *(owner B)*
- **Outcome**: `VM.initLevel()` returns 0 → 1 → 2 → 3 → 4 at real points; `awaitInitLevel(n)` blocks correctly.
- **Files**: `native-builtins/src/jdk_internal.rs`, `vm/src/vm/vm_init.rs`.
- **Acceptance**: `System.getProperty("java.class.path")` at initLevel ≥ 1; `Thread.currentThread().getName()` at ≥ 2.

### WP1.4 — `SharedSecrets` complete bridge  [L, 2-3d]  *(owner B)*
- **Outcome**: all `jdk.internal.access.*Access` interfaces have real bridge implementations.
- **Files**: `vm/src/runtime/shared_secrets.rs`, `native-builtins/src/jdk_internal.rs`.
- **Minimum list**: `JavaLangAccess`, `JavaLangInvokeAccess`, `JavaLangRefAccess`, `JavaLangReflectAccess`, `JavaIOAccess`, `JavaIORandomAccessFileAccess`, `JavaNetInetAddressAccess`, `JavaNetUriAccess`, `JavaNioAccess`, `JavaSecurityAccess`, `JavaUtilJarAccess`, `JavaUtilZipFileAccess`, `JavaNetHttpCookieAccess`, `JavaObjectInputStreamAccess`, `JavaUtilResourceBundleAccess`.
- **Acceptance**: `Class.getProtectionDomain0` reaches via SharedSecrets bridge; `ResourceBundle.getBundle` works; `ZipFile` internal-entry iteration works.

### WP1.5 — `BootLoader` + `BuiltinClassLoader` parity  [L, 3d]  *(owner C)*
- **Outcome**: `ClassLoader.getSystemClassLoader()` returns `AppClassLoader`; `getPlatformClassLoader()` returns `PlatformClassLoader`; boot loader is `null` per spec; `getParent()` chain works.
- **Files**: `classloading/src/builtin_loaders.rs`, `classloading/src/class_manager.rs`.
- **Acceptance**: `ServiceLoader.load(Driver.class)` finds drivers via `META-INF/services/java.sql.Driver` on classpath; `getResource("META-INF/MANIFEST.MF")` returns first match.

### WP1.6 — `java.lang.invoke` complete  [XL, 4-6d]  *(owner C)*
- **Outcome**: every Java 11+ `invokedynamic`-using code path works. MethodHandle chains, VarHandle for array/field/static, LambdaMetafactory generates concrete class, StringConcatFactory.makeConcatWithConstants generates Java-level concat, ConstantBootstraps.
- **Files**: `vm/src/runtime/invokedynamic.rs`, `vm/src/runtime/methodhandle.rs`, `vm/src/runtime/varhandle.rs`, `vm/src/runtime/lambda_proxy.rs`.
- **Minimum acceptance**:
  - `record Foo(int a,int b) {}` — auto-generated `equals`/`hashCode`/`toString` work via `ObjectMethods.bootstrap`.
  - `String s = "a" + b + "c"` — `makeConcatWithConstants` runs.
  - `List.of(1,2,3)` works.
  - `Stream.of(1,2,3).map(i->i*2).collect(Collectors.toList())` works.
  - `VarHandle.acquire/release` on `volatile int` field of a regular class.
  - `MethodHandles.Lookup.findVirtual` + `.bindTo` + `.invokeExact` round-trips.

### WP1.7 — Annotation retention RUNTIME parsing  [M, 2d]  *(owner D)*  ✅ DONE
- **Outcome**: `@Retention(RUNTIME)` annotations on classes/methods/fields/parameters/type-uses parse from the .class file and populate real proxy objects reachable via `getAnnotation(Class)`.
- **Resolution (session 93)**: `apps/annotation_probe` round-trips string/int/class/enum/nested/array/method-anno/field-anno values. Files: `reader/src/class_reader.rs`, `classloading/src/annotations.rs`.

### WP1.8 — `java.util.ServiceLoader` per-module  [M, 1-2d]  *(owner D)*  ⚠️ partial
- **Outcome**: `ServiceLoader.load(Class)` scans `META-INF/services/<fqcn>` on classpath.
- **Status (session 93)**: `native-builtins/src/service_loader.rs` exists; `apps/serviceloader_probe` returns count=0 — classpath scan not wired to JDK iterator.
- **Acceptance**: `ServiceLoader.load(java.sql.Driver.class)` finds H2 (or any driver JAR) via `META-INF/services` on classpath.

### WP1.9 — `java.lang.StackWalker` complete  [M, 1d]  *(owner E)*  ⚠️ partial
- **Outcome**: `StackWalker.getInstance().walk(s -> ...)` returns real `StackFrame` objects with class, method, BCI, line.
- **Status (session 93)**: `vm/src/runtime/stackwalker.rs` exists; `apps/stackwalker_probe` NPEs on missing native `StackStreamFactory.checkStackWalkModes()Z`.
- **Acceptance**: app log lines show correct stack traces (no `<unknown>` frames in prod code).

### WP1.10 — `Cleaner` / `PhantomReference` registration  [M, 1-2d]  *(owner E)*  ⚠️ partial
- **Outcome**: `java.lang.ref.Cleaner` registers cleanup actions run on GC; `PhantomReference` enqueues correctly.
- **Status (session 93)**: `apps/phantom_probe` ✅ phantom path works (created/get-null/enqueue/poll). `apps/cleaner_probe` ❌ — operand-stack tag mismatch ("expected int got double") mid-execution before cleanup runs.
- **Acceptance**: allocate 1M `DirectByteBuffer` in a loop — cleaner reclaims native memory within 2 GCs.

### WP1.11 — `java.lang.System.getenv` / `getProperties` fidelity  [S, 0.5d]  *(owner F)*
- **Outcome**: match HotSpot's exact set of system properties (40+ keys including `java.home`, `java.version=25.0.1`, `os.name`, `os.arch`, `user.dir`, `path.separator`, `file.separator`, `file.encoding`, `stdout.encoding`, `stderr.encoding`, `line.separator`, `java.class.path`, `java.library.path`, `user.country`, `user.language`, `user.home`, `user.name`, `java.specification.name`, vendor keys, `native.encoding`, `sun.jnu.encoding`).
- **Files**: `vm/src/runtime/lang_system.rs`.
- **Acceptance**: `apps/sysprops_probe/` prints same 40+ keys as HotSpot 25.

### WP1.12 — `java.lang.Runtime.exec` + `ProcessBuilder`  [M, 2d]  *(owner F)*
- **Outcome**: `ProcessBuilder.start()` spawns a real child on Windows + Linux; stdin/stdout/stderr plumbed via `InputStream`/`OutputStream`; `process.waitFor()`, `process.exitValue()`, `process.destroy()` work.
- **Files**: `native-io/src/process.rs`.
- **Acceptance**: `apps/process_probe/` runs `cmd /c echo hello` (Windows) / `/bin/echo hello` (Linux) and reads the output; `cat` round-trips stdin; non-zero exit code preserved.

---

## 5. Wave 2 — Reflection + runtime bytecode generation

Prereq: Wave 1. Dispatch: **5 agents**.

### WP2.1 — `java.lang.reflect` full coverage  [L, 3d]
- **Outcome**: every public API on `Class`/`Method`/`Field`/`Constructor`/`Parameter`/`TypeVariable`/`WildcardType`/`GenericArrayType`/`AnnotatedType` works.
- **Files**: `native-builtins/src/lang_reflect.rs`, `vm/src/runtime/lang_class.rs`.
- **Hot list**: `Method.invoke` (boxing/unboxing, varargs, default, abstract throws `AbstractMethodError`, interface static, override check), `Field.get/set` (volatile aware, final check), `Class.getDeclaredMethods(String)`, `getEnclosingClass`, `getNestHost`, `getPermittedSubclasses`, `getRecordComponents`, `isSealed`, `trySetAccessible`, `canAccess`, `getTypeAnnotations`.
- **Acceptance**: ByteBuddy's `TypeDescription.forLoadedType(cls)` parses every standard JDK class without `UnsupportedOperationException`.

### WP2.2 — `Method.invoke` complete  [M, 2d]
- **Outcome**: as part of WP2.1, the reflection hot path works for all descriptor shapes.
- **Files**: `vm/src/runtime/interpreter.rs::native_method_invoke`.
- **Acceptance**: 50-case matrix covering void return, primitive args, object args, boxed primitives, interface default, varargs `Object[]`, exception rethrow wrapping in `InvocationTargetException`.

### WP2.3 — `Unsafe.defineClass` / `Lookup.defineClass` / `ClassLoader.defineClass`  [XL, 4-5d]
- **Outcome**: runtime bytecode generation — ByteBuddy, CGLIB, JDK dynamic Proxy, Weld can generate concrete classes on demand.
- **Files**: `classloading/src/class_manager.rs::define_class_with_options`, `native-builtins/src/unsafe_natives.rs::defineClass`.
- **Acceptance**: `apps/cglib_probe/` runs CGLIB's enhancer pattern and invokes a generated proxy method; ByteBuddy's `new ByteBuddy().subclass(Object.class).make()` produces a real class.

### WP2.4 — `java.lang.instrument` interface  [M, 2d]
- **Outcome**: `Instrumentation.redefineClasses/retransformClasses` accept new bytecode and replace method bodies.
- **Files**: `vm/src/runtime/instrument.rs`, `classloading/src/class_manager.rs::redefine`.
- **Acceptance**: Mockito's MockMaker agent + Jacoco coverage agent both work under rust-jvm.

### WP2.5 — Dynamic `Proxy.newProxyInstance`  [M, 1-2d]
- **Outcome**: JDK dynamic proxy generates real bytecode at runtime (not synthetic stub).
- **Files**: `vm/src/runtime/proxy.rs`, uses WP2.3 `defineClass`.
- **Acceptance**: `java.sql.Connection` proxy intercepts `prepareStatement`; an interface-based proxy intercepts all `@Path`-style methods.

### WP2.6 — `Constructor.newInstance` edge cases  [S, 0.5d]
- **Outcome**: private constructors, inner-class outer-ref injection, record canonical constructors all work.
- **Files**: `vm/src/runtime/lang_reflect_constructor.rs`.
- **Acceptance**: Jackson deserialization of `record Foo(int a)` via canonical constructor works.

### WP2.7 — Annotation proxy via `Annotation.asInterface`  [M, 1-2d]
- **Outcome**: annotation-type proxies returned by `getAnnotation` are real `Proxy` instances whose methods return parsed element-value-pair values.
- **Files**: `classloading/src/annotations.rs`, uses WP2.5.
- **Acceptance**: `@Inject` + `@Named("foo")` are discoverable on fields with `.value().equals("foo")`. Currently a synthetic field-bag from WP1.7 — promote to a real `Proxy`.

### WP2.8 — `Class.getGenericSuperclass` / `getGenericInterfaces`  [S, 1d]
- **Outcome**: parameterized types survive reflection as `ParameterizedType` with `getActualTypeArguments()`.
- **Files**: `reader/src/class_reader.rs::Signature`, `vm/src/runtime/generics.rs`.
- **Acceptance**: Jackson deserializes `List<User>`; Hibernate-style entity-type discovery finds `List<OrderLine>` collections.

### WP2.9 — `MethodHandles.Lookup.findSpecial`  [S, 1d]
- **Outcome**: `Lookup.findSpecial(C,"m",mt,C.class)` returns an invokespecial MH; private-to-private invocation works.
- **Files**: `vm/src/runtime/methodhandle.rs`.
- **Acceptance**: Java 8+ default-method super-call pattern works.

### WP2.10 — Anonymous + hidden class accounting  [S, 1d]
- **Outcome**: `Class.getNestHost` reflects anonymous-class relationships; `isHidden()` true for hidden classes; `Class.forName(hiddenName)` fails with `ClassNotFoundException`.
- **Files**: `classloading/src/class.rs`.

---

## 6. Wave 3 — NIO / async I/O

Prereq: Wave 1. Dispatch: **5 agents**.

### WP3.1 — `Selector` real epoll on Linux / IOCP on Windows  [XL, 5d]
- **Outcome**: `SelectorProvider.provider().openSelector()` creates a platform selector; `select(timeout)` blocks on real kernel wait.
- **Files**: `native-io/src/selector.rs`, platform-gated.
- **Acceptance**: `apps/echo_server/` — 1K concurrent connections, 100K msgs/s; identical ordering to HotSpot.

### WP3.2 — `AsynchronousSocketChannel` / `AsynchronousServerSocketChannel`  [L, 3-4d]
- **Outcome**: NIO.2 async I/O with completion handlers; back-pressure via `CompletionHandler` callbacks.
- **Files**: `native-io/src/async_socket.rs`.
- **Acceptance**: `apps/aio_echo/` — 10K concurrent Netty-style connections.

### WP3.3 — `FileChannel.map` mmap  [M, 2d]
- **Outcome**: memory-mapped files return `MappedByteBuffer` backed by real `mmap` / `MapViewOfFile`.
- **Files**: `native-io/src/file_channel.rs`.
- **Acceptance**: H2 page store opens `*.mv.db` via `FileChannel.map`; arbitrary-size mmap round-trips bytes.

### WP3.4 — `SocketChannel` non-blocking read/write with EAGAIN semantics  [M, 2d]
- **Outcome**: `read()` returns 0 when no data, `write()` returns partial; edge-triggered compatible.
- **Files**: `native-io/src/socket_channel.rs`.
- **Acceptance**: stress test — 10K partial writes reassemble correctly.

### WP3.5 — DirectByteBuffer pooling + Cleaner coop  [M, 1d]
- **Outcome**: `ByteBuffer.allocateDirect(n)` allocates native memory; Cleaner reclaims on collect (WP1.10 integration).
- **Files**: `native-io/src/direct_buffer.rs`.
- **Acceptance**: 1M `allocateDirect(4096)` in a loop — native RSS bounded.

### WP3.6 — `FileChannel.transferTo/transferFrom` zero-copy  [M, 1d]
- **Outcome**: `sendfile(2)` on Linux, `TransmitFile` on Windows; falls back to pipe copy.
- **Files**: `native-io/src/file_channel.rs`.

### WP3.7 — `Pipe.open()` + `DatagramChannel`  [M, 1d]
- **Outcome**: anonymous pipe + UDP channel.
- **Files**: `native-io/src/pipe.rs`, `native-io/src/datagram.rs`.
- **Acceptance**: `apps/pipe_probe/` round-trips a byte; DNS query via `DatagramChannel` resolves `localhost`.

### WP3.8 — `WatchService` file-system notifications  [M, 1d]
- **Outcome**: `WatchService.poll/take` fires on CREATE/MODIFY/DELETE events.
- **Files**: `native-io/src/watch.rs`.
- **Acceptance**: a directory-scanner app picks up new file drops.

---

## 7. Wave 4 — Concurrency primitives

Prereq: Wave 1. Dispatch: **4 agents**.

### WP4.1 — AQS (`AbstractQueuedSynchronizer`) + `ReentrantLock`  [L, 3d]
- **Outcome**: full CAS-based wait queue, fair + non-fair modes, interruptible + timed acquires; no livelock under 64-thread contention.
- **Files**: move from `synthetic-jdk` partial to real JDK 25 impl running on rust-jvm with Unsafe ops complete (WP1.2).
- **Acceptance**: `apps/aqs_stress/` — 64 threads acquire+release 1M times each; fairness bounded.

### WP4.2 — `CompletableFuture` chaining + common pool  [L, 3d]
- **Outcome**: all CF methods (`thenApply/thenCompose/thenAccept/exceptionally/whenComplete/orTimeout`) with async/sync variants; common pool sized from `Runtime.availableProcessors()`.
- **Files**: synthetic-jdk → real.
- **Acceptance**: CF chain of 100 stages with mix of async/sync completes in <1s.

### WP4.3 — `ForkJoinPool` work-stealing  [L, 3d]
- **Outcome**: `ForkJoinTask.fork/join/invoke` + `RecursiveTask`/`RecursiveAction` with real deque-based stealing.
- **Acceptance**: parallel-array-sum of 1M ints scales ≥4× on 8-core.

### WP4.4 — `LockSupport.park(blocker,nanos)` + `park(Object,long)`  [S, 1d]
- **Outcome**: park/unpark identical to HotSpot — spurious wakeup tolerance, `getBlocker()` returns the last park-arg.
- **Files**: `vm/src/threading/park.rs`.

### WP4.5 — `ScheduledExecutorService` with `scheduleAtFixedRate` + `scheduleWithFixedDelay`  [M, 2d]
- **Outcome**: DelayQueue-based scheduling with monotonic clock; no drift > 10ms at 100Hz.
- **Acceptance**: 100Hz task runs for 60s with jitter < 10ms p99.

### WP4.6 — `ConcurrentHashMap` full coverage  [M, 1-2d]
- **Outcome**: largely working post session-92 livelock fix; re-check `computeIfAbsent` / `merge` / `forEach` under heavy contention.
- **Acceptance**: 64-thread 10M op stress — no livelock, correct element counts.

### WP4.7 — `StampedLock` + `ReadWriteLock` fairness  [M, 1d]
- **Outcome**: optimistic read lock + upgrade works; no reader starvation.
- **Acceptance**: `apps/stamped_stress/`.

### WP4.8 — Virtual threads complete  [L, 3d]
- **Outcome**: `Thread.ofVirtual().start(r)` runs real virtual threads on a carrier pool; `Thread.sleep` yields correctly; pinning on `synchronized`/JNI works.
- **Files**: `vm/src/threading/virtual_threads.rs` (expand from session 92 `sleep0` fix).
- **Acceptance**: 1M virtual threads concurrent, total native threads ≤ cores × 2.

---

## 8. Wave 5 — Net + TLS

Prereq: Wave 1, Wave 3. Dispatch: **5 agents**.

### WP5.1 — `SSLEngine` server + client RFC 8446  [XL, 5d]
- **Outcome**: full TLS 1.3 with client-auth (mandatory client-cert), 1.2 fallback, cipher suite negotiation, session resumption.
- **Files**: `tls/src/server.rs`, `tls/src/client.rs` (expand from session 87 atomic-flight fix).
- **Acceptance**: `openssl s_client` connects with `--cert` mTLS; handshake completes; all 40 RFC 8446 test vectors pass.

### WP5.2 — `KeyStore` real PKCS12 + JKS parse  [L, 3d]
- **Outcome**: load `keystore.p12` / `keystore.jks` with passwords; extract keys + certs.
- **Files**: `native-builtins/src/keystore.rs`.

### WP5.3 — `X509KeyManager` / `X509TrustManager` per-connection  [M, 1-2d]
- **Outcome**: key-manager selects cert alias by key-usage + ext-key-usage; trust manager does full chain + CRL.
- **Files**: `tls/src/x509_manager.rs`.

### WP5.4 — ALPN for HTTP/2  [M, 1-2d]
- **Outcome**: TLS ALPN extension negotiates `h2` / `http/1.1`.
- **Files**: `tls/src/alpn.rs`.

### WP5.5 — `HttpClient` (JDK 11+) HTTP/1.1 + HTTP/2  [L, 3d]
- **Outcome**: `java.net.http.HttpClient` works with sync + async sends; HTTP/2 frames + multiplexing.
- **Acceptance**: `HttpClient.send(GET https://example.com)` round-trips.

### WP5.6 — `HttpURLConnection` legacy path  [M, 1-2d]
- **Outcome**: legacy `url.openConnection()` works.

### WP5.7 — `Socket` / `ServerSocket` plain TCP  [M, 1d]
- **Outcome**: blocking classical I/O works with `SO_REUSEADDR`, `TCP_NODELAY`, `SO_KEEPALIVE` options.
- **Acceptance**: 1K concurrent HTTP/1.0 requests on port 8080.

### WP5.8 — DNS resolver (`InetAddress.getAllByName`)  [M, 1d]
- **Outcome**: real `getaddrinfo` backing.
- **Files**: `native-builtins/src/inet_address.rs`.
- **Acceptance**: A+AAAA resolution; reverse PTR.

### WP5.9 — Proxy selector + `NO_PROXY`  [S, 0.5d]
- **Outcome**: `ProxySelector.getDefault()` honours JVM args + env.

---

## 9. Wave 6 — JCE / crypto provider chain

Prereq: Waves 1, 5. Dispatch: **5 agents**.

### WP6.1 — JCE provider chain + algorithm registration  [L, 3d]
- **Outcome**: `Security.getProviders()` returns SUN + SunJCE + SunEC + BC after BC install; `MessageDigest.getInstance("SHA-256")` dispatches through first matching provider.
- **Files**: `native-builtins/src/jca/provider_chain.rs`.
- **Acceptance**: 40+ algos return non-null Instance; round-trip SHA-256, HMAC-SHA-256, AES-GCM, RSA-OAEP, ECDSA-P256, Ed25519.

### WP6.2 — `MessageDigest` full algo set  [M, 2d]
- **Outcome**: SHA-256/384/512, SHA3-256/384/512, MD5 (legacy), SHA-1 (legacy).
- **Acceptance**: CAVP test vectors per algo.

### WP6.3 — `Cipher` full algo set  [L, 3d]
- **Outcome**: AES-CBC/CTR/GCM, ChaCha20-Poly1305, RSA-OAEP, RSA-PKCS1, DES (legacy).
- **Acceptance**: NIST test vectors per algo.

### WP6.4 — `Signature` + `KeyFactory` + `KeyGenerator`  [L, 3d]
- **Outcome**: RSA-PKCS1/PSS, ECDSA (secp256r1/384r1/521r1), EdDSA (Ed25519/Ed448), DSA.
- **Acceptance**: sign + verify per algo; RFC 8032, RFC 8410 test vectors.

### WP6.5 — BouncyCastle compatibility  [L, 3d]
- **Outcome**: BC JAR (`bcprov-jdk18on-1.80.jar`) loads, registers, and all 500+ algos work because they only need JVM primitives.
- **Acceptance**: `BouncyCastleProvider.getName().equals("BC")` + round-trip DN parse via `X500Name`.

### WP6.6 — ASN.1 DER encode/decode  [M, 2d]
- **Outcome**: `sun.security.util.DerValue` / BC's `ASN1Sequence` produce byte-exact output vs HotSpot.
- **Acceptance**: PKCS#10 CSR encode + decode round-trip.

### WP6.7 — `SecureRandom` + `Random` quality  [S, 1d]
- **Outcome**: `SecureRandom` uses OS `getrandom` / `BCryptGenRandom`; `Random` matches Sun impl deterministically.
- **Files**: `native-builtins/src/securerandom.rs`.
- **Acceptance**: NIST SP 800-22 sanity tests pass for 1MB of bytes.

### WP6.8 — `java.security.Policy` production parse  [M, 1d]
- **Outcome**: real `java.policy` files load without silent downgrade.
- **Files**: `native-builtins/src/security_manager/policy.rs` (extend session 86).
- **Acceptance**: app boots under `-Djava.security.manager` succeeds.

---

## 10. Wave 7 — JDBC + ServiceLoader-based SPI

Prereq: Wave 1 (esp. WP1.8). Dispatch: **2 agents**.

> Most JDBC drivers (H2, PostgreSQL, MySQL, SQLite-JDBC, MariaDB) are pure-Java JARs and run on a spec-compliant JVM unchanged. The JVM-level work here is about **driver discovery via SPI** and **JDBC core types being reachable**, not about implementing any specific driver.

### WP7.1 — `DriverManager` + ServiceLoader registration  [S, 0.5d]
- **Outcome**: `DriverManager.getConnection("jdbc:h2:mem:test")` finds H2 Driver via SPI (depends on WP1.8).
- **Files**: `native-builtins/src/jdbc.rs` (if needed) — most should work via existing classloading.
- **Acceptance**: smoke-test against H2 in-mem DB; identical against PostgreSQL embedded fixture.

### WP7.2 — `java.sql.*` core types reachable  [S, 0.5d]
- **Outcome**: `Connection`, `Statement`, `PreparedStatement`, `ResultSet`, `Driver`, `DatabaseMetaData` all load + reflect properly under WP2.1.
- **Files**: synthetic-jdk audit — these may need real-JDK promotion.

### WP7.3 — `java.sql.Types` + `Date/Time/Timestamp`  [S, 0.5d]
- **Outcome**: legacy SQL date types + new `java.time.*` driver paths interoperate.
- **Acceptance**: insert + select round-trips `LocalDateTime` via H2 standard `TIMESTAMP` column.

---

## 11. Wave 8 — Stabilization, perf, CI

Prereq: Waves 1-7 baseline. Dispatch: **5 agents**.

### WP8.1 — 24h soak under representative load  [L, 3d]
- **Outcome**: heap growth ≤ 2× initial; no FD/native-mem leak; GC pause p99 < 50ms.
- **Acceptance**: at least one of the S2 apps under 10 ops/s for 24h, clean exit.

### WP8.2 — Concurrent perf vs HotSpot  [L, 3d]
- **Outcome**: bench-hotspot-compare geomean ≤ 2× HotSpot 25 on DaCapo + Renaissance subset.

### WP8.3 — TLS edge cases  [M, 1-2d]
- **Outcome**: session resumption under load; client-cert renegotiation; 0-RTT early data.

### WP8.4 — Bytecode verifier full coverage  [M, 1-2d]
- **Outcome**: every class verifies under `-Xverify:all`. JDK module-info handled.

### WP8.5 — HotSpot-parity tracing  [M, 1d]
- **Outcome**: `-XX:+PrintGC`, `-Xlog:class+load=trace`, `-Xlog:gc*=info` produce comparable output.

### WP8.6 — Profile-guided optimization  [M, 1-2d]
- **Outcome**: capture boot + steady-state profile of S2 apps; feed into tiered JIT.

### WP8.7 — Forcing-function CI matrix  [L, 2-3d]
- **Outcome**: GitHub Actions matrix runs each S2 app's smoke fixture nightly (~1h budget per matrix slot). Track regressions per fixture against the schema-v1 baseline (`bench/<app>/bench-baseline.json`).

### WP8.8 — Documentation  [M, 2d]
- **Outcome**: `docs/INSTALL.md`, `docs/CONFIG.md`, per-app runbooks. Keep this roadmap honest as work lands.

---

## 12. Dispatch strategy

**Agent cohort sizes per wave (recommended):**

- Waves 0, 7 — 2-5 agents (small).
- Waves 1, 4, 5, 8 — 4-6 agents.
- Waves 2, 3, 6 — 5 agents.

**Rules (from session 88+93 lessons):**
1. Grep-verify file paths before dispatch — session 88 lost 5h × 2 on a non-existent `concurrency.rs`.
2. Bundle each WP with anchor-grep acceptance: "function X exists at `path:line`" or "test Y exists in `test_file.rs`".
3. One wave at a time; never parallelize across waves unless each WP explicitly declares no cross-wave dep.
4. Report back at each WP boundary with test diff (before/after per-crate counts).
5. Mutex on `Cargo.toml` dep edits — one agent owns deps per wave.
6. **Rate-limit awareness (session 93)**: opus agents on Wave 1 hit Anthropic rate-limit ~20-60 min in. Plan for 6-agent waves to consume real wall-clock budget; consider sonnet for cheaper waves or stagger dispatches.

**Per-agent briefing template:**
```
You own WP<id>. Read docs/wildfly-ejbca-roadmap.md §<wave>.
Surface area: <files>. Anchor-grep acceptance: <greps>. Blocked on: <deps>.
Write/edit only within surface area. When done, report test delta + anchor-grep evidence.
```

---

## 13. Test harness

Reusable scripts committed under `bench/`:

- `bench/<app>/stage.{sh,ps1}` — fetch + stage a forcing-function fixture.
- `bench/<app>/run-under-rustjvm.{sh,ps1}` — boot under `target/release/rustjvm.exe`.
- `bench/<app>/bench-baseline.json` — schema-v1 pinned acceptance.
- `bench/<app>/diff-baseline.{sh,ps1}` — exit non-zero on any drift.

CI jobs (WP8.7):
- `forcing-function-smoke-nightly` — matrix over S2 apps.
- `soak-weekly` — 24h soak + memory graph.

---

## 14. Risk list

Risks ordered by likelihood × impact:

1. **Bytecode-generation infra at Wave 2** — if WP2.3 (`defineClass`) is harder than estimated, DI/proxy frameworks break. Mitigation: start WP2.3 as the first Wave 2 WP; wire a minimal CGLIB probe early.
2. **TLS internals** — `sun.security.ssl.*` is private API with HotSpot-specific idioms. Mitigation: lean on `SSLEngine` interface only; avoid private-API divergence.
3. **Reflection hot paths** — if WP2.1 leaves gaps, app boot hangs in cryptic ways. Mitigation: probe each app with a representative reflection fixture before declaring Wave 2 done.
4. **mmap semantics** — apps relying on `FileChannel.map` (H2, indexed-search) have specific expectations. Mitigation: fall back to `read/write` if mmap edge-cases persist.
5. **Operand-stack tag mismatches** — recurring family (sessions 87, 91, 93). Each new native-bridge return path is a risk. Mitigation: descriptor-aware `push_compact(CompactValue::long/double/...)` per session-91 K1-K6 hardening.

---

## 15. What is NOT in scope

Explicitly out of scope — these are *test fixtures*, not JVM work:

- WildFly subsystems, JBoss Modules parser, JBoss MSC, jboss-deployment-structure.xml — pure-Java JARs.
- Hibernate / JPA, Weld / CDI, Mojarra / JSF, RESTEasy / JAX-RS, Undertow, Narayana, IronJacamar — pure-Java JARs.
- EJBCA-specific features (CA init, ManagementCA, mTLS admin enrolment) — application logic, not JVM.
- Liquibase / Flyway migrations — JARs.
- TCK certification — requires Oracle license; we target spec-compliance equivalence.

These run on a spec-compliant JVM unchanged. When one of them breaks, that breakage is a **forcing function** revealing a JDK 25 gap (see Wave 0-7 above).

---

## 16. Change log

- **2026-04-24 (session 93)** — re-scoped from "WildFly + EJBCA on rust-jvm" (155 WPs across 20 waves) to "JDK 25 compat for any Java app" (~71 WPs across 9 waves). Removed 84 WPs that were really JAR-replacement (JBoss Modules, MSC, subsystems, EJB container, EJBCA build/deploy/tests). Kept wave structure. Authoritative roadmap pointer in `memory/MEMORY.md`.
- **2026-04-24 (earlier session 93)** — initial WildFly+EJBCA version. Scope rejected by user feedback (`memory/feedback_wildfly_is_jdk_compat.md`): "you generally do not need to modify the JVM itself for WildFly to run — it's configuration, not JVM edits."
