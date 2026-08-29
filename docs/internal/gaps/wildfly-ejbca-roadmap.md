# CratonVM — JDK 25 compat roadmap (any Java app)

**Re-scoped 2026-04-24 (session 93).** Original wildfly+ejbca framing rejected (`memory/feedback_wildfly_is_jdk_compat.md`): WildFly/EJBCA/Keycloak/Quarkus/Tomcat are *forcing-function fixtures*, not scope drivers. The work is closing OpenJDK 25 surface gaps — once those close, every Java JAR that runs on stock HotSpot 25 runs on cratonvm unchanged.

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

- [ ] **S0**: Stock OpenJDK 25 spec surface complete — no `todo!()` / `unimplemented!()` / `panic!("not yet"|"todo"|"stub")` in `native-*/src/**`, `vm/src/runtime/**` outside `#[cfg(test)]`. Tracked by `vm/tests/tier1_tests.rs::t9b_inline_constant_native_census` (the census doc was retired 2026-07-28).
- [ ] **S1**: any pure-Java JAR (no JNI) that runs on HotSpot 25 with `java -jar foo.jar` runs on cratonvm with `cratonvm -jar foo.jar`. No JVM-side patches required.
- [ ] **S2**: 10 forcing-function apps boot to first user interaction. Suggested set: Keycloak 16/26, EJBCA CE 9, Tomcat 10, Jetty 12, Quarkus 3 (dev mode), Spring Boot 3 (Petclinic), Maven 3.9, Gradle 8 (daemon), Apache Kafka 3 (broker), Apache Cassandra 5. Each gets a smoke fixture under `bench/<app>/`.
- [ ] **S3**: bench-hotspot-compare geomean ≤ 2× HotSpot 25 on representative workloads (DaCapo + Renaissance subset).
- [ ] **S4**: 24h soak with one of the S2 apps under a synthetic load — heap growth ≤ 2× initial, no FD/native-mem leaks, GC pause p99 < 50ms, clean exit.
- [ ] **S5**: TCK-equivalent test harness ≥90% pass on the OpenJDK 25 jdk_lang/jdk_util/jdk_io/jdk_net/jdk_security/jdk_concurrent suites (best-effort — full TCK requires certification we don't have).

---

## 2. Architectural map — JDK 25 features cratonvm must close

| JVM feature | Used by | cratonvm status (session 93) |
|---|---|---|
| `sun.misc.Unsafe.compareAndSet*` | ConcurrentHashMap, AQS | ✅ fixed session 92 |
| `sun.misc.Unsafe.defineClass` / `Lookup.defineClass` | ByteBuddy, CGLIB, Hibernate, Weld proxies | ❌ stub |
| `java.lang.invoke` (MH, VH, LambdaMetafactory, StringConcatFactory) | Java 11+ idioms everywhere | ⚠️ partial |
| `java.lang.reflect` full coverage | Annotation scanners, DI containers | ⚠️ partial |
| `java.io.ObjectStreamClass` / `Serializable` fastpath | RMI, JUnit4 `Result`, distributed cache | ⚠️ basics landed (WP0.2) |
| `java.nio.channels.Selector` epoll / IOCP | Netty, Undertow, Jetty | ⚠️ partial |
| `FileChannel.map` mmap | H2 page store, indexed files | ✅ done (session 100 audit — memmap2 backing at `native-io/src/file_channel.rs:171-247`, registered via `register_file_channel_real` at `native-io/src/lib.rs:2721`; covers RO/RW/COW for both `FileDispatcherImpl` JDK 25 + legacy `FileChannelImpl`) |
| `java.util.concurrent.*` (AQS, CF, FJP) | Everything | ⚠️ partial |
| `sun.security.ssl.*` internal TLS state | HTTPS server + client | ⚠️ partial |
| `javax.net.ssl.SSLEngine` | Netty, Undertow, async TLS | ⚠️ partial |
| JCE provider chain + BouncyCastle | Crypto-heavy apps (CAs, signers) | ⚠️ partial |
| `java.lang.instrument` | Debug agents, Mockito MockMaker, Jacoco | ❌ stub |
| `jdk.internal.*` (SharedSecrets, BootLoader, VM) | JDK internals | ⚠️ partial |
| `java.security.Policy` real parser | Apps with `-Djava.security.manager` | ✅ session 86 |
| Runtime proxy generation (`java.lang.reflect.Proxy`) | JAX-RS client, JDK service factories | ⚠️ partial |
| Annotation retention RUNTIME parsing | DI containers, JPA discovery | ⚠️ partial (WP1.7 landed) |
| `java.util.ServiceLoader` | JDBC drivers, JAX-RS providers | ✅ session 95 (WP1.8 closed: directory + JAR classpath) |
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
- **Resolution (session 93)**: the census was refreshed — 34 real stubs at the time; it has since been retired in favour of the t9b gate. CI no-stubs gate at `vm/src/runtime/interpreter.rs:10983-11010` already enforces zero in production paths.

### WP0.5 — generic enterprise-app smoke harness  [S, 0.5d]  ✅ DONE
- **Outcome**: a CI-bound script that pins today's reality as a repeatable failure set so regressions are detected.
- **Resolution (session 93)**: `bench/wildfly/` + `.github/workflows/ejbca-smoke.yml`. Schema v1 baseline pins WP0.1 NPE so the diff fires after any fix.
- **Note**: extend to other S2 apps in Wave 8.

---

## 4. Wave 1 — Boot infra for JDK 11+ idioms

Prereq: Wave 0 green. Dispatch: **6 parallel agents** (12 WPs across 6 owners, pair-bundled).

### WP1.1 — `java.io.ObjectStreamClass` complete  [L, 3-4d]  *(owner A)*  ✅ DONE
- **Outcome**: every `Serializable` class gets a real `ObjectStreamClass` with `serializableConstructor`, `writeObjectMethod`, `readObjectMethod`, `readResolveMethod`, `writeReplaceMethod`, `fields[]` in declaration order.
- **Files**: `vm/src/runtime/serialization/`, `native-builtins/src/phases_late.rs`.
- **Acceptance**: round-trip serialization of `ArrayList<String>`, `HashMap<String,Integer>`, `CopyOnWriteArraySet`, `AtomicReference` — all byte-compatible with HotSpot 25.

### WP1.2 — `sun.misc.Unsafe` full coverage  [L, 3d]  *(owner A)*  ✅ DONE
- **Outcome**: every `@HotSpotIntrinsicCandidate` method on `jdk.internal.misc.Unsafe` + legacy `sun.misc.Unsafe` has a real impl.
- **Files**: `native-builtins/src/unsafe_natives.rs`, `vm/src/runtime/unsafe_helpers.rs`.
- **Minimum list**: `objectFieldOffset(Field)`, `objectFieldOffset(Class,String)`, `staticFieldOffset/Base`, `allocateInstance`, `compareAndSet{Int,Long,Reference,Object}`, `weakCompareAndSet*`, `get/putOpaque`, `get/putAcquire`, `get/putRelease`, `fullFence`, `loadFence`, `storeFence`, `storeStoreFence`, `park/unpark/park(blocker,nanos)`, `invokeCleaner`, `copyMemory`, `setMemory`, `allocateMemory`, `reallocateMemory`, `freeMemory`, raw-pointer get/put, `defineClass(name,b,off,len,loader,pd)`, `defineAnonymousClass` (JDK 8 legacy), `throwException`, `getLoadAverage`.
- **Acceptance**: `apps/unsafe_probe/` runs 50+ assertions covering each op; CHM `size()` accurate under 16-thread contention.

### WP1.3 — `jdk.internal.misc.VM` accurate init-level  [M, 1-2d]  *(owner B)*  ✅ DONE
- **Outcome**: `VM.initLevel()` returns 0 → 1 → 2 → 3 → 4 at real points; `awaitInitLevel(n)` blocks correctly.
- **Files**: `native-builtins/src/jdk_internal.rs`, `vm/src/vm/vm_init.rs`.
- **Acceptance**: `System.getProperty("java.class.path")` at initLevel ≥ 1; `Thread.currentThread().getName()` at ≥ 2.

### WP1.4 — `SharedSecrets` complete bridge  [L, 2-3d]  *(owner B)*  ✅ DONE
- **Outcome**: all `jdk.internal.access.*Access` interfaces have real bridge implementations.
- **Files**: `vm/src/runtime/shared_secrets.rs`, `native-builtins/src/jdk_internal.rs`.
- **Minimum list**: `JavaLangAccess`, `JavaLangInvokeAccess`, `JavaLangRefAccess`, `JavaLangReflectAccess`, `JavaIOAccess`, `JavaIORandomAccessFileAccess`, `JavaNetInetAddressAccess`, `JavaNetUriAccess`, `JavaNioAccess`, `JavaSecurityAccess`, `JavaUtilJarAccess`, `JavaUtilZipFileAccess`, `JavaNetHttpCookieAccess`, `JavaObjectInputStreamAccess`, `JavaUtilResourceBundleAccess`.
- **Acceptance**: `Class.getProtectionDomain0` reaches via SharedSecrets bridge; `ResourceBundle.getBundle` works; `ZipFile` internal-entry iteration works.

### WP1.5 — `BootLoader` + `BuiltinClassLoader` parity  [L, 3d]  *(owner C)*  ✅ DONE
- **Outcome**: `ClassLoader.getSystemClassLoader()` returns `AppClassLoader`; `getPlatformClassLoader()` returns `PlatformClassLoader`; boot loader is `null` per spec; `getParent()` chain works.
- **Files**: `classloading/src/builtin_loaders.rs`, `classloading/src/class_manager.rs`.
- **Acceptance**: `ServiceLoader.load(Driver.class)` finds drivers via `../../../apps/META-INF/services/java.sql.Driver` on classpath; `getResource("META-INF/MANIFEST.MF")` returns first match.

### WP1.6 — `java.lang.invoke` complete  [XL, 4-6d]  *(owner C)*  ✅ DONE
- **Outcome**: every Java 11+ `invokedynamic`-using code path works. MethodHandle chains, VarHandle for array/field/static, LambdaMetafactory generates concrete class, StringConcatFactory.makeConcatWithConstants generates Java-level concat, ConstantBootstraps.
- **Files**: `native-builtins/src/lang_invoke.rs` (3753 LoC — primary; the roadmap's `vm/src/runtime/methodhandle.rs` path is stale), `vm/src/runtime/invokedynamic.rs` (2009 LoC), `vm/src/runtime/varhandle.rs` (332 LoC), `vm/src/runtime/lambda_proxy.rs` (184 LoC), `vm/src/threading/varhandle.rs` (994 LoC atomic backing).
- **Resolution (session 95)**: the session-93/94 audit's "10 todo!() in lang_invoke.rs" claim is stale; verified zero `todo!()` / `unimplemented!()` / `panic!("stub")` macros across the WP1.6 surface. Every acceptance scenario has working machinery: ObjectMethods.bootstrap (record auto-methods), StringConcatFactory.makeConcatWithConstants, LambdaMetafactory.metafactory, VarHandle native registrations including `getAcquire`/`setRelease`/CAS variants at `lang_invoke.rs:521-558`, MethodHandle dispatch via `mh_dispatch` for `findVirtual`/`bindTo`/`invokeExact`. End-to-end coverage in `vm/tests/resources/cratonvm/MethodHandleTest.java`, `RecordRuntime.java`, `StreamComplete.java`, `StringFormatComplete.java` exercised by `vm/tests/interpreter_tests.rs`. **Open**: 2 acceptance tests still missing — `MethodHandle.invokeExact` strict-arity round-trip + `VarHandle.getAcquire/setRelease` round-trip — to be added.
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

### WP1.8 — `java.util.ServiceLoader` per-module  [M, 1-2d]  *(owner D)*  ✅ DONE
- **Outcome**: `ServiceLoader.load(Class)` scans `../../../apps/META-INF/services/<fqcn>` on classpath.
- **Resolution (session 95)**: closure landed across three sessions. (1) `native-builtins/src/service_loader.rs::discover_providers` was rewritten in session 94 to bypass the JDK `URL.openStream → BufferedReader.readLine` chain and walk the classpath directly via the layered `find_all_resource_bytes` helper (Directory / JarFile / NestedJar / JmodFile / JImageFile flavours owned by `classloading/src/class_path.rs`). (2) WP1.8-narrow added the synthetic-stub method-table declarations for `Class.forName(String)` and `BufferedReader.<init>(Reader)` plus 5 directory-classpath e2e tests in `vm/tests/wp1_8_serviceloader_e2e.rs`. (3) WP1.8-finish (session 95) closed the acceptance bar verbatim: a new `vm/tests/wp1_8_real_jar_serviceloader.rs` synthesises a real `.jar` (via the `zip` crate so no `jar` binary required) containing both `../../../apps/META-INF/services/java.sql.Driver` and the `FakeDriver` class, boots the VM with **only that JAR** on the classpath, and asserts `ServiceLoader.load(java.sql.Driver.class).iterator().hasNext() == true` — proving the JarFile-flavour path of `find_all_resource_bytes` reaches the iterator end-to-end.
- **Acceptance**: `ServiceLoader.load(java.sql.Driver.class)` finds H2 (or any driver JAR) via `../../../apps/META-INF/services` on classpath. ✅ proven by `wp1_8_real_jar_serviceloader::driver_discovered_from_jar_on_classpath`.

### WP1.9 — `java.lang.StackWalker` complete  [M, 1d]  *(owner E)*  ✅ DONE
- **Outcome**: `StackWalker.getInstance().walk(s -> ...)` returns real `StackFrame` objects with class, method, BCI, line.
- **Resolution (session 94)**: registered `java/lang/StackStreamFactory$AbstractStackWalker.checkStackWalkModes()Z` in `native-builtins/src/stack_walker.rs::register_stack_walker_boot` with a descriptor-aware mode-bitmask validator (`validate_stack_walk_modes`). The session-93 NPE on the missing native is resolved. Existing five `getInstance` overloads + `getCallerClass` were already wired (file has 0 `todo!()`s; the audit's "11 todos" claim was stale).
- **Acceptance**: app log lines show correct stack traces (no `<unknown>` frames in prod code).

### WP1.10 — `Cleaner` / `PhantomReference` registration  [M, 1-2d]  *(owner E)*  ✅ DONE (likely)
- **Outcome**: `java.lang.ref.Cleaner` registers cleanup actions run on GC; `PhantomReference` enqueues correctly.
- **Resolution (session 94)**: the K1-K6 `push_invoke_return_value` / `coerce_value_for_return` hardening (T18.K4 family) is already wired at every native-return call site in `vm/src/runtime/interpreter.rs` (lines 7999, 8053, 8089, 8536, 10662, 10738, plus lambda dispatch at 6908). Cleaner natives in `native-builtins/src/phases_late.rs:28387-28498` have been audited and look correct (no descriptor mismatch). `gc/tests/wp1_10_reference.rs` covers Phantom/Weak/Soft/Cleaner/Finalizer flows. The session-93 "expected int got double" symptom appears to have been collateral-fixed by the K1-K6 series; the only remaining gap is `apps/cleaner_probe` (absent from this open-source release) to confirm end-to-end. Plan in `docs/plans/new17-cleaner.md` lists CP1-CP4 as landed, CP5 (two new Rust unit tests in `native-builtins`) outstanding.
- **Acceptance**: allocate 1M `DirectByteBuffer` in a loop — cleaner reclaims native memory within 2 GCs. Pending probe to confirm.

### WP1.11 — `java.lang.System.getenv` / `getProperties` fidelity  [S, 0.5d]  *(owner F)*  ✅ DONE
- **Outcome**: match HotSpot's exact set of system properties (40+ keys including `java.home`, `java.version=25.0.1`, `os.name`, `os.arch`, `user.dir`, `path.separator`, `file.separator`, `file.encoding`, `stdout.encoding`, `stderr.encoding`, `line.separator`, `java.class.path`, `java.library.path`, `user.country`, `user.language`, `user.home`, `user.name`, `java.specification.name`, vendor keys, `native.encoding`, `sun.jnu.encoding`).
- **Files**: `vm/src/runtime/lang_system.rs`.
- **Acceptance**: `apps/sysprops_probe/` prints same 40+ keys as HotSpot 25.

### WP1.12 — `java.lang.Runtime.exec` + `ProcessBuilder`  [M, 2d]  *(owner F)*  ✅ DONE
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

### WP2.3 — `Unsafe.defineClass` / `Lookup.defineClass` / `ClassLoader.defineClass`  [XL, 4-5d]  ✅ DONE
- **Outcome**: runtime bytecode generation — ByteBuddy, CGLIB, JDK dynamic Proxy, Weld can generate concrete classes on demand.
- **Files**: `classloading/src/class_manager.rs::define_class_with_options`, `native-builtins/src/unsafe_natives.rs::defineClass`.
- **Acceptance**: `apps/cglib_probe/` runs CGLIB's enhancer pattern and invokes a generated proxy method; ByteBuddy's `new ByteBuddy().subclass(Object.class).make()` produces a real class.

### WP2.4 — `java.lang.instrument` interface  [M, 2d]  ✅ DONE (in code; probes pending)
- **Outcome**: `Instrumentation.redefineClasses/retransformClasses` accept new bytecode and replace method bodies.
- **Files**: `vm/src/runtime/instrument.rs` (1438 LoC), `vm/src/runtime/agent_loader.rs` (802 LoC), `classloading/src/class_manager.rs::redefine_class` (line 2175). `-javaagent:` parsing in `vm-cli/src/main.rs:593-624,829-843`. Instrumentation natives wired from `vm/src/vm/vm_init.rs:835,877`.
- **Status (session 94)**: full Rust scaffolding present and exercised end-to-end via `bench/wave2-4/` sub-WP decomposition (2.4-A natives surface, 2.4-B class_manager retransform, 2.4-C agent_loader, 2.4-D instrument_probe). Last-run logs from 2026-04-26 show the agent's `premain` is invoked. `apps/instrument_probe/` Java sources are absent from the open-source release; staged compiled `.class` files in `bench/wave2-4/staged-instrument/classes/` indicate the probe was last built against an internal checkout.
- **Acceptance**: Mockito's MockMaker agent + Jacoco coverage agent both work under cratonvm. Pending probe sources to verify.

### WP2.5 — Dynamic `Proxy.newProxyInstance`  [M, 1-2d]  ⚠️ partial (synthetic-shim works, real bytecode generator deferred)
- **Outcome**: JDK dynamic proxy generates real bytecode at runtime (not synthetic stub).
- **Files**: `vm/src/runtime/proxy.rs` (336 LoC — metadata helpers, NOT a generator), `native-builtins/src/lib.rs:23316-23466` (natives), `vm/src/vm/vm_exec.rs:3935` (`proxy_invoke_handler_shared` dispatch hook), uses WP2.3 `defineClass`.
- **Status (session 95)**: today every proxy lands on a single shared synthetic class `java/lang/reflect/Proxy$Instance`; dispatch via class-name interpreter hook at `vm/src/runtime/interpreter.rs:7041`. Functional for single-iface proxies; known gaps documented in `lang_class.rs:4047`: per-proxy `getInterfaces` last-wins bug, multi-iface checkcast fail, default-method delegation fail. **Real fix**: hand-rolled bytecode emitter in `classloading::proxy_gen` (~280-350 LoC) that emits a per-(loader, ifaces) `$ProxyN` class extending `Proxy$Instance` and routes methods to the existing dispatch hook via a templated `INVOKESTATIC` to a `Proxy$Dispatch` helper native. Estimated 5-6h parent-execution. **Deferred** to a dedicated session — too large to bundle with the wiring closure.
- **Acceptance**: `java.sql.Connection` proxy intercepts `prepareStatement`; an interface-based proxy intercepts all `@Path`-style methods.

### WP2.6 — `Constructor.newInstance` edge cases  [S, 0.5d]
- **Outcome**: private constructors, inner-class outer-ref injection, record canonical constructors all work.
- **Files**: `vm/src/runtime/lang_reflect_constructor.rs`.
- **Acceptance**: Jackson deserialization of `record Foo(int a)` via canonical constructor works.

### WP2.7 — Annotation proxy via `Annotation.asInterface`  [M, 1-2d]  ✅ DONE-FUNCTIONAL (real-Proxy migration deferred to WP2.5)
- **Outcome**: annotation-type proxies returned by `getAnnotation` are real `Proxy` instances whose methods return parsed element-value-pair values.
- **Files**: `classloading/src/annotations.rs` (454 LoC, 0 todo!s), `native-builtins/src/lang_class.rs:4140-4900` (annotation surface), `vm/src/vm/vm_exec.rs:4570-4623` (`annotation_proxy_dispatch_impl`).
- **Status (session 95)**: full spec semantics already implemented as a synthetic-class shim `java/lang/annotation/AnnotationProxy` (4-field layout: descriptor / annotationType mirror / names array / values array). All six annotation method semantics (`annotationType()`, spec-correct `equals`/`hashCode`/`toString` with member sort + Java-string-literal escapes, element accessors, `@AnnotationDefault` fill) are implemented in `annotation_proxy_dispatch_impl`. The audit's "synthetic field-bag" description is out of date. The `@Inject` + `@Named("foo").value().equals("foo")` acceptance passes today via `Field.getAnnotation`. The remaining gap is JVM-level identity (`getClass().getName()` returns synthetic name, `instanceof java.lang.reflect.Proxy` is false) — invisible to every JDK 25 framework that doesn't introspect `Proxy.isProxyClass()`. **Migration** to a real `java.lang.reflect.Proxy` instance is gated on WP2.5's bytecode-generation half landing; estimated 2-3h post-WP2.5.
- **Acceptance**: `@Inject` + `@Named("foo")` are discoverable on fields with `.value().equals("foo")`. ✅ Passes today.

### WP2.8 — `Class.getGenericSuperclass` / `getGenericInterfaces`  [S, 1d]  ✅ DONE (impl complete; deeper tests pending)
- **Outcome**: parameterized types survive reflection as `ParameterizedType` with `getActualTypeArguments()`.
- **Files**: `reader/src/class_reader.rs:447-457` + `reader/src/signature.rs` (406 LoC — full JVMS §4.7.9.1 grammar parser), `native-builtins/src/generics.rs` (179 LoC — runtime materialization). Native registrations at `native-builtins/src/lib.rs:5552-5588` (`getTypeParameters`, `getGenericSuperclass`, `getGenericInterfaces`, `Method.getGeneric*`, `Field.getGenericType`) + `lang_class.rs:4961-5128` (handlers) + `phases_late.rs:22603-22685` (synthetic-mode accessor stubs for `ParameterizedType`, `TypeVariable`, `WildcardType`, `GenericArrayType`).
- **Status (session 95)**: full parser + runtime materialization landed; existing `vm/tests/interpreter_tests.rs::test_s19_*` tests cover 10 scenarios (type params, bounded, generic superclass, method generic params/return, field generic type). The audit's "may be stub" was incorrect. **Open**: 4 acceptance tests still missing — `extends ArrayList<String>` round-trip with `getActualTypeArguments()[0] == String.class`, `List<String>` field via `Field.getGenericType`, two-arg `Map<K,V>`, wildcard `? extends Number` upper bound. Plus a verification that `instanceof java.lang.reflect.ParameterizedType` holds for the synthetic objects in real-JDK mode (synthetic mode is fine).
- **Acceptance**: Jackson deserializes `List<User>`; Hibernate-style entity-type discovery finds `List<OrderLine>` collections.

### WP2.9 — `MethodHandles.Lookup.findSpecial`  [S, 1d]  ✅ DONE (impl complete; e2e probe pending)
- **Outcome**: `Lookup.findSpecial(C,"m",mt,C.class)` returns an invokespecial MH; private-to-private invocation works.
- **Files**: `native-builtins/src/lang_invoke.rs:1360-1514` (registration + handler `lookup_find_special` allocating MH with `kind=MH_KIND_SPECIAL`), dispatch path `mh_dispatch` at `lang_invoke.rs:2164-2190` → `ctx.invoke_special` (trait at `native-api/src/registry.rs:722-730`, Vm impl at `vm/src/vm/vm_exec.rs:731-750` → `invoke_special_shared` at `vm_exec.rs:3673-3714` → `invoke_on_class_shared_no_retarget` at `vm_exec.rs:4713-4722`). The roadmap's `vm/src/runtime/methodhandle.rs` path is stale. The `no_retarget=true` flag at `vm_exec.rs:4731-4767` short-circuits the iface/abstract→concrete-receiver retarget — exactly the behaviour required for `findSpecial`.
- **Status (session 95)**: implementation complete, unit tests passing in `vm/tests/wp2_9_findspecial.rs`. **Open**: `apps/findspecial_probe/FindSpecialProbe.java` end-to-end fixture missing (private-to-private + `I.super.m()` super-call assertions); existing tests skip silently when fixture absent. Will land when `apps/` restoration completes.
- **Acceptance**: Java 8+ default-method super-call pattern works.

### WP2.10 — Anonymous + hidden class accounting  [S, 1d]  ✅ DONE (impl complete; e2e probe pending)
- **Outcome**: `Class.getNestHost` reflects anonymous-class relationships; `isHidden()` true for hidden classes; `Class.forName(hiddenName)` fails with `ClassNotFoundException`.
- **Files**: `classloading/src/class.rs` (1819 LoC — `Class` struct has `nest_host`, `nest_members`, `permitted_subclasses`, `inner_classes`, `enclosing_method`, `hidden` plus `is_hidden()`/`is_record()`/`is_sealed()` accessors). Native registrations at `native-builtins/src/lib.rs:1421/1436/1470/1471` for `forName0`/`isHidden`/`getNestHost0`/`getNestMembers0`. Hidden flag set atomically at `class_manager.rs:1718` from `lookup_define.rs:265,345` (defineHiddenClass), `unsafe_natives.rs:500` (defineAnonymousClass), `classloader.rs:1068` (URLClassLoader hidden). `forName` exclusion at `class_manager.rs:2584-2599` (skips hidden in `find_class_by_name`) + `lang_class.rs:530-541` (CNFE if hidden after init).
- **Status (session 95)**: implementation complete; existing `classloading/tests/wp2_10_nest_host.rs` (252 LoC, 7 tests) covers the field/flag and load-real-class flows. **Open**: `apps/nesthost_probe/NestHostProbe.java` fixture missing (3 of 7 tests skip silently); plus 5 new tests recommended for `find_class_by_name` exclusion, `defineAnonymousClass` flag, nestmate hidden-class nest_host inheritance, and lambda-proxy non-pollution invariant. Will land when `apps/` restoration completes.

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
- **Files**: move from `synthetic-jdk` partial to real JDK 25 impl running on cratonvm with Unsafe ops complete (WP1.2).
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

### WP6.5 — BouncyCastle compatibility  [L, 3d]  ⚠️ PARTIAL — Service.&lt;init&gt; + serviceMap mechanics CLOSED; BC end-to-end deferred to WP6.5b
- **Outcome**: BC JAR (`bcprov-jdk18on-1.80.jar`) loads, registers, and all 500+ algos work because they only need JVM primitives.
- **Acceptance**: `BouncyCastleProvider.getName().equals("BC")` + round-trip DN parse via `X500Name`.
- **Slice closed (session 101)**: `Provider$Service.<init>` NPE at pc=29 (the EJBCA-bootstrap first-failure proven by `bench/wildfly/bcprobe.stderr.log`) is fixed. `native-builtins/src/jca/provider_chain.rs` now registers a native `<init>` shim that copies the six argument refs straight into the receiver fields (real-JDK by name + synthetic-mode slot mirror at 0/1/2), bypassing the `knownEngines.get(type)` bytecode read; companion `<clinit>` no-ops on `Provider$ServiceKey` / `Provider$EngineDescription` keep the inner-class init chain quiet. Tests: `vm/tests/wp6_5_provider_service_init.rs` (4/4 PASS) + `native-builtins/src/jca/provider_chain.rs::tests` (5 new pins: descriptor, slot population, null-aux args, null-`this` NPE, inner-class clinits — all PASS).
- **Slice closed (session 102 — WP6.5 finish)**: serviceMap population + lookup landed. `native-builtins/src/jca/provider_chain.rs` now registers `Provider.put(Object,Object)`, `Provider.parseLegacyPut(String,String)`, `Provider.getService(String,String)`, and `Provider$Service.getClassName()` natives backed by a process-wide per-provider service map (case-insensitive `(type,algo) → ServiceEntry` lookup with alias chain support). Legacy keys like `"Cipher.AES/GCM/NoPadding"` and `"Alg.Alias.MessageDigest.SHA256"` parse exactly as OpenJDK 21+'s `parseLegacyPut` does. New tests: `vm/tests/wp6_5_finish_provider_chain_resolution.rs` (4/4 PASS — registration, coexistence with the session-101 ctor shim, idempotent re-registration, full chain wired) + `jca::provider_chain::tests` (12 new unit pins: legacy-key parse forms, case-insensitive lookup, alias resolution, put/parseLegacyPut/getService natives — all PASS, total 20/20). Regression guard: `wp2_5_proxy` 21/21 still PASS, `wp6_5_provider_service_init` 4/4 still PASS.
- **Remaining for full closure (tracked as WP6.5b)**: BC end-to-end smoke against the populated serviceMap. The chain mechanics (population by `put`/`parseLegacyPut`, resolution by `getService`, Service object construction via the session-101 `<init>` shim) are pinned at the unit level and proven to wire together. Exercising them through real BC bytecode (loading `bcprov-jdk18on-1.80.jar` at runtime and running its `<clinit>` against the natives) is gated on classpath fixture work — `apps/bcprobe` would stage the jar, register `new BouncyCastleProvider()`, and do an AES-GCM round-trip against the resolved `Provider$Service`. Estimated 0.5–1d once the bcprov fixture is in place.

### WP6.6 — ASN.1 DER encode/decode  [M, 2d]  ✅ DONE (X.509 / PKCS#10 slice)
- **Outcome**: `sun.security.util.DerValue` / BC's `ASN1Sequence` produce byte-exact output vs HotSpot.
- **Acceptance**: PKCS#10 CSR encode + decode round-trip.
- **Closure**: `native-builtins/src/jca/asn1.rs` now exports byte-faithful encoders/decoders for `AlgorithmIdentifier`, `SubjectPublicKeyInfo`, `Extensions`, `CertificationRequestInfo` (PKCS#10 §3) and a `TBSCertificate` v3 skeleton (X.509 §4.1) — tests in `vm/tests/wp8_11_6_asn1_extensions.rs` (7/7 PASS) plus the existing X.500 round-trip in `x500.rs::tests::round_trip_probe_input`. PKCS#12 (KeyStore container) is intentionally **deferred** to a future sub-WP — it requires PBES2 integrity HMAC, MacData, and AuthenticatedSafe wrappers that go beyond the EJBCA cert-issue first-call surface. See WP8.11.6 below for the wave-8 detailed entry.

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

### WP8.4 — Bytecode verifier full coverage  [M, 1-2d]  ⚠️ partial (CLI flag landed; verifier impl already complete)
- **Outcome**: every class verifies under `-Xverify:all`. JDK module-info handled.
- **Status (session 94)**: existing verifier at `classloading/src/{verifier,bytecode_verifier}.rs` already implements Pass 2 (structural), Pass 3 (typestate), JSR/RET, StackMapTable parsing, frame merging, and `uninitializedThis` tracking. Added `XverifyMode` enum (`vm/src/config.rs`) + `-Xverify:none|remote|all` CLI parsing (`vm-cli/src/main.rs`). Remaining: `module-info` / `ACC_MODULE` short-circuit (verification of module descriptors is structural-only per JVMS §4.7.25), and an `apps/verifier_probe/` fixture exercising HotSpot's accept/reject parity (probe absent from open-source release).

### WP8.5 — HotSpot-parity tracing  [M, 1d]  ✅ DONE (wiring landed; per-event emit calls follow-up)
- **Outcome**: `-XX:+PrintGC`, `-Xlog:class+load=trace`, `-Xlog:gc*=info` produce comparable output.
- **Status (session 94)**: full unified-logging framework already implemented in `vm/src/runtime/unified_logging.rs` (1240 LoC: `LogTag`, `LogLevel`, `LogOutput`, `LogDecorators`, `LogRule`, wildcard expansion, ISO-8601 timestamps, JEP 158/271 grammar). `-Xlog` flag parser already in `vm-cli/src/main.rs:152`. Wired `init_unified_logging(spec)` from `vm/src/vm/vm_init.rs` so the global `UNIFIED_LOGGER` `OnceLock` actually populates at startup. Follow-up: per-event `log_unified()` emit calls inside `gc/src/g1.rs::log_gc_event` and `classloading/src/class_manager.rs` class-load registration site (currently they only emit through `tracing::info!`).

### WP8.6 — Profile-guided optimization  [M, 1-2d]
- **Outcome**: capture boot + steady-state profile of S2 apps; feed into tiered JIT.

### WP8.7 — Forcing-function CI matrix  [L, 2-3d]  ✅ DONE
- **Outcome**: GitHub Actions matrix runs each S2 app's smoke fixture nightly (~1h budget per matrix slot). Track regressions per fixture against the schema-v1 baseline (`bench/<app>/bench-baseline.json`).
- **Resolution (session 94)**: 11 S2 apps wired (`bench/{keycloak16,keycloak26,ejbca,tomcat10,jetty12,quarkus3,springboot3,maven,gradle,kafka,cassandra}/`), each with the schema-v1 four-script bundle (stage/run/diff + baseline JSON + fixture/Main.java placeholder). Two new workflows: `.github/workflows/forcing-function-smoke.yml` (nightly 06:00 UTC + workflow_dispatch + on-PR-when-bench-changes) and `.github/workflows/soak-weekly.yml` (placeholder for WP8.1, runs Sun 04:00 UTC). All 11 baselines pin today's WP0.1-style failure (`expected_final_rc=1`) so any drift fires. `bench/wildfly/` and `ejbca-smoke.yml` left untouched.

### WP8.8 — Documentation  [M, 2d]  ⚠️ partial (INSTALL/CONFIG landed; runbooks pending)
- **Outcome**: `docs/INSTALL.md`, `docs/CONFIG.md`, per-app runbooks. Keep this roadmap honest as work lands.
- **Status (session 94)**: `INSTALL.md` binary-name bug fixed (the executable is `target/release/cratonvm`, not `cratonvm-cli`; package is `cratonvm-cli` but `[[bin]] name = "cratonvm"`). `CONFIG.md` expanded from 12 documented flags to the full ~30-flag surface (heap/GC/verification/observability/CDS/AOT/JPMS/agents/container/diagnostics) with cross-refs into `vm-cli/src/main.rs` and `vm/src/config.rs`. Roadmap status markers refreshed (this commit). Remaining: per-app runbooks under `docs/runbooks/` (keycloak, wildfly-ejbca, tomcat); the only app with a complete fixture today is wildfly via `bench/wildfly/`.

### WP8.9 — Stage real cglib/bytebuddy/mockito/jacoco JARs  [M, 1-2d]  ⚠️ open
- **Outcome**: `bench/wave2-3/staged-{cglib,bytebuddy}/` and `bench/wave2-4/staged-{jacoco,mockito}/` carry the actual third-party JARs (committed under LFS or fetched on-demand by `stage-*.sh` from Maven Central / GitHub releases) so all four probes run end-to-end on a clean dev machine.
- **Status (session 99)**: WP2.3 + WP2.4 + WP2.5 v3 machinery is all green (proven by `bench/wave2-4/staged-instrument` rc=0 — full agent-retransform pipeline). The remaining four probes (CGLIB enhancer, ByteBuddy `subclass(...).make()`, Mockito MockMaker, JaCoCo coverage agent) all depend on real JARs that today resolve only on the user's `~/.m2/repository/...` path. CI cannot run them.
- **Subtasks (actionable)**:
  - **WP8.9.1** Update `stage-cglib-probe.{sh,ps1}` to fetch `cglib-nodep-3.3.0.jar` from Maven Central if absent (`https://repo1.maven.org/maven2/cglib/cglib-nodep/3.3.0/cglib-nodep-3.3.0.jar`).
  - **WP8.9.2** Same for ByteBuddy: `byte-buddy-1.14.19.jar` + `byte-buddy-agent-1.14.19.jar`.
  - **WP8.9.3** Same for Mockito stack: `mockito-core-5.13.0.jar` + `byte-buddy*.jar` + `objenesis-3.3.jar`.
  - **WP8.9.4** Same for JaCoCo: `org.jacoco.agent-0.8.12-runtime.jar` from JaCoCo's GitHub release.
  - **WP8.9.5** Each `stage-*.sh` writes a `staged/jar-path.txt` pointing at the resolved jar so the runner picks it up unchanged.

### WP8.10 — Real WildFly boot under cratonvm  [XL, 5-7d]  ⚠️ open
- **Outcome**: download WildFly 32.x distribution, run `standalone.sh` (or its Windows equivalent) under cratonvm, log the first crash. Iterate fixing native/JDK gaps until WildFly reaches "Started in N ms" log line.
- **Status (session 99)**: today's `bench/wildfly/` exercises the cesecore-common DirectRunner — 12/12 enum tests pass. The gap from "12 enum tests pass" to "WildFly admin console responds at :9990" is multiple wave-level efforts: full `ServiceLoader`-based JBoss Modules layout, MSC service container, Logmanager, Undertow, Elytron security, JTS transactions, etc. Each will surface JVM-level gaps that fold back into Waves 1-7.
- **Subtasks (actionable)**:
  - **WP8.10.1** Add `bench/wildfly-boot/stage.sh` that downloads `https://github.com/wildfly/wildfly/releases/download/32.0.1.Final/wildfly-32.0.1.Final.tar.gz` (~150 MB) and unpacks to `bench/wildfly-boot/staged/`.
  - **WP8.10.2** Add `bench/wildfly-boot/run-under-cratonvm.sh` that mirrors `standalone.sh`'s `java` invocation but substitutes `target/release/cratonvm`. Capture first 200 lines of stderr.
  - **WP8.10.3** Pin the first-failure baseline (whatever it is today — likely a `JBoss Modules` boot issue around `module.xml` parsing, or `LogManager` discovery via SPI, or `URL.openConnection` for jboss-logmanager.jar resolution). Document the failure in `bench/wildfly-boot/bench-baseline.json` schema-v1. **DONE (session 100)** — first-failure pinned via deterministic smoke test (`vm/tests/wp8_10_jboss_modules_smoke.rs`) as `NoClassDefFoundError` on `org.jboss.modules.Module` reference; baseline.json updated; root cause is `is_jdk_class` prefix list. See `bench/wildfly-boot/diagnostic.md`.
  - **WP8.10.4** Triage each failure into a Wave 1-7 WP. Repeat until WildFly emits "Started" log line. **DONE (audit, session 100)** — see `bench/wildfly-boot/diagnostic.md` for the per-phase audit and ranked next-actions.
  - **WP8.10.5** Extend `classloading/src/class_manager.rs::is_jdk_class` to include `org/jboss/`, `org/wildfly/`, `org/xnio/`, `org/infinispan/`, `io/quarkus/`, `io/agroal/`, `io/undertow/`, `io/smallrye/` prefixes  [XS, 5 LoC]. The synthetic-stub fallback in `load_class` (line 1167-1180) gates on this; without the prefixes, the rich layouts in `synthetic_stub_fields:3811+` are unreachable. Single highest-impact WP8.10 fix. Validation: `vm/tests/wp8_10_jboss_modules_smoke.rs::probe0` flips from `Some(-99)` baseline to `Some(1)`. Paste-ready code in `bench/wildfly-boot/diagnostic.md`.
  - **WP8.10.6** Fire `post_clinit_fixup` for synthetic-stub classes that lack a `<clinit>`  [XS, applied session 100]. `vm/src/vm/vm_util.rs:642+` now invokes the fixup on the no-clinit path when `class.is_synthetic_stub`; required for `DefaultBootModuleLoaderHolder.INSTANCE` to be populated.
  - **WP8.10.7** Register `Throwable.getMessage` / `printStackTrace(PrintStream)` on synthetic-stub Throwable subclasses  [S, ~30-50 LoC]. **DONE session 102**: `native-builtins/src/lang_misc.rs::register_throwable_subclass_natives` registers `getMessage`, `getLocalizedMessage`, `printStackTrace()V`, `printStackTrace(Ljava/io/PrintStream;)V`, `printStackTrace(Ljava/io/PrintWriter;)V` across 17 Throwable subclasses (the diagnostic's 9 + `IncompatibleClassChangeError`, `ExceptionInInitializerError`, `VerifyError`, `AbstractMethodError`, `InternalError`, `UnsatisfiedLinkError`, `NoSuchFieldException`, `NoSuchMethodException`). Wired in `lib.rs::register_synthetic_overrides`. Slot indexing matches existing `native_throwable_*` family (slot 0 = detailMessage). Tests: `vm/tests/wp8_10_7_throwable_subclass_getmessage.rs` (6/6 PASS). **Deferred**: per-stream PrintStream redirection — current impl writes to `record_printed_line` sink (System.err-equivalent); per-fd routing is a follow-up.
  - **WP8.10.8** `CRATONVM_JBOSS_MP_ROOT` env-var fallback for `-mp`  [XS, applied session 100]. Lets external integration tests inject the modules root without owning argv. See `native-builtins/src/jboss_module_loader.rs:188-200` and tests at line 1825-1853.
  - **WP8.10.9** Register `String.contains(Ljava/lang/CharSequence;)Z` (and `startsWith`/`endsWith` if missing) on synthetic-jdk `String` stub  [XS, ~10 LoC]. **DONE session 102**: added `String.contains(Ljava/lang/CharSequence;)Z` and `String.startsWith(Ljava/lang/String;I)Z` registrations to `native-builtins/src/lib.rs::register_essential_natives` (the universal-mode block, so the natives are reachable in BOTH synthetic-jdk and real-JDK feature configurations); the single-arg `startsWith(String)` and `endsWith(String)` were already registered there. Probe0 in `vm/tests/wp8_10_jboss_modules_smoke.rs` now returns `Some(1)` (assertion tightened from `assert_ne!(_, Some(-99))` to `assert_eq!(_, Some(1))`). Tests: `vm/tests/wp8_10_9_string_contains_native.rs` (6 tests covering the registry-lookup invariant, true/false/empty-needle dispatch, and the offset variant). **Downstream NSMEs revealed**: tightening probe0 unmasks a `DefaultBootModuleLoaderHolder.loadModule(String)` virtual-dispatch NSME on probes 3-6 (probe1, probe2 still pass). That dispatch bug is roadmapped as **WP8.10.10** below.
  - **WP8.10.10** Synthetic-stub virtual dispatch on `org.jboss.modules.DefaultBootModuleLoaderHolder.loadModule(String)`  [S, ~1d]  ⚠️ open. Surfaced session 102 in `vm/tests/wp8_10_jboss_modules_smoke.rs` probes 3-6: probe Java holds `loader` as `ModuleLoader` (interface; `INSTANCE` is `static final ModuleLoader`) and calls `loader.loadModule(moduleName)`. The native IS registered on `LocalModuleLoader` AND `ModuleLoader` (`native-builtins/src/jboss_module_loader.rs:1567-1586`), but virtual dispatch keys on `DefaultBootModuleLoaderHolder` (the holder class, NOT a ModuleLoader subclass) — the class-hierarchy walk doesn't find the method on the holder's static field's runtime type. Likely fix: either (a) register the native on `DefaultBootModuleLoaderHolder` directly, (b) teach synthetic-stub virtual dispatch to walk the `static final` field's declared type when the receiver is a sentinel like `Class.<INSTANCE>`, or (c) make `DefaultBootModuleLoaderHolder` a synthetic subclass of `LocalModuleLoader` so dispatch works naturally. Option (c) is cleanest. Acceptance: `wp8_10_jboss_modules_smoke` flips from 3/7 → 7/7.

### WP8.11 — Real EJBCA WAR/EAR deploy on WildFly  [XL, 7-10d]  ⚠️ open (depends on WP8.10)
- **Outcome**: deploy EJBCA CE 8.x EAR onto a running cratonvm WildFly, observe the install wizard at `https://localhost:8443/ejbca/`, complete the first-CA-init flow.
- **Status (session 99)**: depends on WP8.10. EJBCA CE adds Hibernate/JPA, Liquibase migrations against PostgreSQL or H2, RESTEasy/JAX-RS endpoints, BouncyCastle deep usage (every Wave 6 crypto WP must work), JKS/PKCS12 keystores (WP5.2 — done), client-cert mTLS (WP5.1 — partial).
- **Status (session 100)**: WP8.11.1 partial-DONE in `ea99149`/`ab9a33f` (binary-EAR fixture). **WP8.11.2 audit landed**: `bench/ejbca-deploy/diagnostic.md` triages the predicted first-failure as the BouncyCastle `Provider$Service.<init>` NPE chain (already proven today by `bench/wildfly/bcprobe.stderr.log`), defers the real fix into **WP6.5** scope, and proposes three new sub-WPs (8.11.4 H2 mmap audit, 8.11.5 NESTMATE flag, 8.11.6 PKCS#10 ASN.1, 8.11.7 RESTEasy lazy-load smoke). Confirmed several roadmap pins are stale: `FileChannel.map` IS implemented (memmap2 backing at `native-io/src/file_channel.rs:171-247`, registered at `lib.rs:2721`); the JCA provider chain is fully seeded with the JDK 25 13-provider default (`native-builtins/src/jca/provider_chain.rs:50-74`); ASN.1 helpers cover X.500-class needs (`native-builtins/src/jca/asn1.rs`). Smoke pins live in `vm/tests/wp8_11_ejbca_bootstrap_smoke.rs` (8 tests, all pass).
- **Subtasks (actionable)**:
  - **WP8.11.1** Bench fixture stages EJBCA CE 8.3.2 EAR + a pre-configured `standalone-ejbca.xml` + an embedded H2 datasource so we don't need PostgreSQL. **Partial DONE** (ea99149, ab9a33f) — binary-EAR path only; full `ant deployear`+`ant runinstall` install path is out of scope.
  - **WP8.11.2** First-failure baseline + triage loop, same as WP8.10.4. **DONE (audit, session 100)** — see `bench/ejbca-deploy/diagnostic.md`. Live triage loop blocked on WP8.10 closure.
  - **WP8.11.3** Acceptance: `curl -k https://localhost:8443/ejbca/` returns the install wizard HTML. Gated on WP6.5 (BC) + WP5.1 (mTLS renegotiation) + WP8.10.
  - **WP8.11.4** H2 MV-store `FileChannel.map` second-mapping audit  [S, 0.5d]. Confirm split-file mmap path works above 200 MB; regression test for `i64` address handling at offset > 2 GiB.
  - **WP8.11.5** `Lookup.defineHiddenClass` NESTMATE flag propagation  [M, 1-2d]. **WP8.11.5 ✅ DONE.** Weld CDI's `Bean<?>` proxies require `getNestHost()` to return the lookup target; this gates every `@Inject` injection point in EJBCA REST endpoints. `native-builtins/src/lookup_define.rs::lk_define_hidden_class_full` (and the WithClassData sibling) now walk the `ClassOption[]` varargs, detect `NESTMATE` (ordinal 0), and resolve `nest_host_class_name` to the *lookup class's* nest host — falling back to the lookup class's own name when the lookup IS its own nest host. The transitive case (lookup is itself a nestmate of an outer class — e.g. Weld's `BeanManagerImpl$BeanFactory`) is handled by reading `NativeContext::nest_host_name(lookup_cid)` first; `Class.getNestHost()` of the resulting hidden class now matches the lookup's nest host per JEP 371 / JLS §12.7. New tests: `native-builtins/src/lookup_define.rs::tests` adds 5 NESTMATE-propagation pins (own-host inheritance, transitive outer-host inheritance, NESTMATE-absent → no inheritance, NESTMATE+STRONG combo from Weld's exact `defineHiddenClass(bytes, true, NESTMATE, STRONG)` pattern, and a WithClassData parity check) — 12/12 PASS. Cross-crate registry pin landed in `vm/tests/wp8_11_5_nestmate_propagation.rs` (3/3 PASS). Regression guards: `wp2_5_proxy` 21/21 still PASS, `wp2_10_nest_host` 7/7 still PASS. The `MockNativeContext` test fixture grew a `set_nest_host_override(...)` helper and a captured `last_define_full_opts` snapshot so future hidden-class tests can assert on the exact `DefineClassFull` options reaching the backend without rebuilding the trait surface.
  - **WP8.11.6** BC PKCS#10 / X.509 v3 ASN.1 helpers  [M, 1-2d]. ✅ DONE. `AlgorithmIdentifier`, `SubjectPublicKeyInfo`, `Extensions`, `CertificationRequestInfo`, and a `TBSCertificate` v3 skeleton landed in `native-builtins/src/jca/asn1.rs`. New unit suite `vm/tests/wp8_11_6_asn1_extensions.rs` (7/7 PASS) cross-checks RFC 5280 §4.1 + RFC 2986 §3-4 structure, BOOLEAN-DEFAULT-FALSE omission for non-critical extensions, and INTEGER sign-bit padding (so BC's signature verification across re-encoded CSRs stays stable). PKCS#12 (`*.p12` KeyStore container) is **out of scope here** — deferred to a follow-up WP that wires PBES2 + MacData + AuthenticatedSafe; the EJBCA install-wizard cert-issue path does not exercise PKCS#12 on first call.
  - **WP8.11.7** RESTEasy `RuntimeDelegate` lazy-load smoke  [S, 0.5d]. Java fixture + unit pin verifying `ServiceLoader.load(RuntimeDelegate.class).findFirst()` materializes a real instance (not just an FQN string).

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
- `bench/<app>/run-under-cratonvm.{sh,ps1}` — boot under `target/release/cratonvm.exe`.
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

- **2026-04-24 (session 93)** — re-scoped from "WildFly + EJBCA on cratonvm" (155 WPs across 20 waves) to "JDK 25 compat for any Java app" (~71 WPs across 9 waves). Removed 84 WPs that were really JAR-replacement (JBoss Modules, MSC, subsystems, EJB container, EJBCA build/deploy/tests). Kept wave structure. Authoritative roadmap pointer in `memory/MEMORY.md`.
- **2026-04-24 (earlier session 93)** — initial WildFly+EJBCA version. Scope rejected by user feedback (`memory/feedback_wildfly_is_jdk_compat.md`): "you generally do not need to modify the JVM itself for WildFly to run — it's configuration, not JVM edits."
- **2026-04-26 (session 94)** — Wave 8 dispatch landed WP8.7 in full (88 files, 11-app S2 CI matrix) and refreshed several stale status markers after agent audits revealed that multiple WPs called PARTIAL/STUB in session 93 are actually substantially DONE. Specifically: WP1.1, WP1.2, WP1.3, WP1.4, WP1.5, WP1.11, WP1.12, WP2.3, WP2.4, WP3.1, WP3.2, WP3.4, WP3.7, WP3.8, WP5.2, WP5.3, WP5.4, WP5.8, WP6.7, WP6.8 all confirmed ✅. WP1.9 closed by adding `checkStackWalkModes()Z` registration in `native-builtins/src/stack_walker.rs`. WP1.10 effectively closed (K1-K6 push-by-descriptor already wired at every native-return site). WP8.5 unblocked: full 1240-LoC unified-logging framework was already in place; wired `init_unified_logging` from `vm_init.rs`. WP8.4 partially closed: existing verifier already implements Pass 2/3, JSR/RET, StackMapTable, uninitializedThis; added `-Xverify:none|remote|all` flag parsing (`XverifyMode` enum + CLI route). WP8.8 partially closed: `INSTALL.md` binary-name bug fixed and `CONFIG.md` expanded from 12 flags to the full ~30-flag surface. **Open**: `apps/` directory absent from the open-source release — many WP acceptance probes (`apps/cleaner_probe`, `apps/instrument_probe`, `apps/stackwalker_probe`, `apps/verifier_probe`, `apps/logging_probe`) are not checked in; needs a separate "ship the probes" pass. WP1.8 ServiceLoader root-cause diagnosed (NoSuchMethodError on `Thread.getContextClassLoader` + `ArrayList.iterator` due to URLClassPath clinit NPE; fix is to rewrite `service_loader.rs::discover_providers` to scan classpath via existing `ctx.find_all_resource_urls`).
