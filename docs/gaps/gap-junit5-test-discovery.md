# Gap: JUnit5 test discovery blockers

**Affects:** Any JUnit5 test suite run directly under CratonVM (kafka-clients, Spring, Tomcat, any external project)  
**Severity:** High — blocks complete test suite runs for all three app targets  
**Discovered:** 2026-06-10 while attempting kafka-clients test suite  
**Status:** FIXED 2026-06-10 — branch `fix/junit5-test-discovery` (worktree C:\craton\CratonVM-junit5disc)

> **Resolution summary.** All three discovery blockers fixed + 7 execution-phase
> bugs found behind them. Discovery parity with HotSpot on all flows
> (selectClasspathRoots, selectPackage-from-JAR, selectClass). Validated on
> the branch merged with dev @ aac45ade: regression pool 18/18,
> common.serialization 66/66 (=HS), common.header 9/9 (=HS, both root-scan
> and jar selectPackage), common.config 106/114 (HS 114 — remaining 8 are
> kafka-config execution bugs: MockFileConfigProvider allowlist, lenient
> Class.forName accepting bogus names, class-alias resolution, ConfigDef
> ValidString/list-default rendering, LONG parse of empty value;
> String.split(,-1) verified correct).
>
> * **Bug 1** — `FileSystemProvider.newFileSystem(URI,Map)` native registered
>   (mounts jar-backed FS); `Files.readAttributes` made jarfs-aware (real
>   `FileTreeWalker` bytecode now walks mounted jars; the p98 walkFileTree
>   natives are synthetic-only and NOT active in real-JDK builds).
> * **Bug 2** — `Path.of(URI)` native now probes slot 4 + slot 0 of the
>   synthetic URI layouts (toUri writes path at slot 4; by-name reads land
>   out-of-bounds on 5-slot synthetics). Also `relativize` separator-normalizes
>   its result and `FileSystem.getSeparator` returns "/" for jar-mounted FS —
>   without this the scanner built FQCNs with slashes and every class failed
>   the package filter (0 tests discovered).
> * **Bug 3 (as filed)** — no longer reproduces; selectClass works. The
>   original silent rc=1s were partly the cross-session
>   `taskkill /IM cratonvm.exe` artifact (use a uniquely-named binary copy).
>   What remained behind it:
>   - AnnotationProxy member calls (`ExtendWith.value()`) → AbstractMethodError:
>     rescues added in interpreter no-Code path + vm_exec NSME path.
>   - `Parameter` objects built with synthetic layout → real `getType()` read a
>     Class mirror as `executable` → `Class.getSharedParameterTypes` NSME:
>     build_parameter_array now populates the real JDK layout by name.
>   - JEP-403 over-enforcement: PUBLIC ctor/field reflection demanded `opens`
>     (kafka ListDeserializer "Could not construct a list instance").
>   - **JIT**: lambda-proxy receiver in `jit_invoke_virtual_mic` resolved class
>     name "" → message-less NoClassDefFoundError once the calling method got
>     compiled (`CollectionUtils.forEachInReverseOrder` — any 3+-class run).
>     Helper now routes lambda receivers through `try_lambda_dispatch`.
>   - `ArrayList.equals` never called user `equals` (UUID lists unequal) and
>     cross-type List equals (ArrayList vs LinkedList) was false; LinkedList's
>     snapshot ListItr was bound to the REAL `LinkedList$ListItr` class whose
>     field coercion broke cursor writes (renamed to internal class).
>   - `IntStream.mapToObj` native added (jupiter-params primitive @ValueSource).

---

## Summary

Three separate CratonVM bugs prevent the JUnit5 Launcher API from discovering or running test classes. Together they completely block running kafka-clients, Spring Boot, or Tomcat test suites under CratonVM via a programmatic JUnit5 launcher.

The commons-math full-reactor test suite runs under CratonVM only because Maven surefire **forks a separate CratonVM process per test class** via its booter mechanism — it does not use the JUnit5 classpath scanner at all.

---

## Bug 1 — `FileSystemProvider.newFileSystem(URI, Map)` has no Code attribute

**Trigger:** `selectPackage("org.apache.kafka")` or any package-based discovery

**Error:**
```
Caused by: java.lang.AbstractMethodError: method java/nio/file/spi/FileSystemProvider.newFileSystem(Ljava/net/URI;Ljava/util/Map;)Ljava/nio/file/FileSystem; has no Code attribute
```

**Root cause:** JUnit5's classpath scanner calls `FileSystems.newFileSystem(jarUri, Map.of())` to open JAR entries as a `ZipFileSystem` (for iterating class files inside the JAR). CratonVM's synthetic `FileSystemProvider` abstract class has `newFileSystem(URI, Map)` declared but no native or bytecode body registered, so calling it throws `AbstractMethodError`.

**Fix direction:** Register a native `FileSystemProvider.newFileSystem(URI, Map)` that delegates to the real JDK ZipFileSystemProvider for `jar:` URIs, or remove the abstract declaration and let the real JDK implementation run.

---

## Bug 2 — `NIO Path.exists()` returns false for Windows absolute paths

**Trigger:** `selectClasspathRoots(Set.of(Paths.get("C:/craton/tmp/kafka-test-classes")))` 

**Error:**
```
Caused by: org.junit.platform.commons.PreconditionViolationException: baseDir must exist: 
```

**Root cause:** JUnit5's `ClasspathRootSelector` checks that the directory exists via `path.toFile().exists()` or `Files.exists(path)`. For a Windows absolute path `C:/craton/tmp/kafka-test-classes`, CratonVM's NIO path implementation returns false (the path renders as blank in the error message, suggesting `toString()` or `toUri()` also fails). The directory genuinely exists and HotSpot finds it fine.

**Fix direction:** Fix `java.nio.file.Path` → `File.exists()` / `Files.exists()` for Windows drive-letter absolute paths under CratonVM's NIO layer.

---

## Bug 3 — NPE in `MethodSelector` resolution during `Class.getDeclaredMethods()`

**Trigger:** `selectClass("org.apache.kafka.common.serialization.SerializationTest")`

**Error:**
```
Caused by: org.junit.platform.commons.JUnitException: MethodSelector [className='...SerializationTest', methodName='testDeserializeVoid', parameterTypes=''] resolution failed
Caused by: java.lang.NullPointerException: Cannot invoke equals on null
```

**Root cause:** JUnit5's `MethodSelector` resolves a test class's methods via reflection. The `Cannot invoke equals on null` NPE suggests that when iterating `Class.getDeclaredMethods()` on a test class, some method attribute (return type, parameter type, or annotation value) is null when it shouldn't be. This is likely in CratonVM's `Method.getReturnType()` or `Method.getParameterTypes()` returning null elements for methods loaded from the kafka-clients test JAR.

**Fix direction:** Audit `Class.getDeclaredMethods()` reflection — ensure method return type and parameter types are never null (default to `Object.class` for unresolved types). This is the same general area as prior reflection fixes in the keycloak/Jackson constructor annotation work.

---

## HotSpot baseline — kafka-clients unit tests

Using programmatic JUnit5 launcher with kafka-clients-3.7.1-test.jar:

| Package | Tests | Pass | Fail | Wall (HS) |
|---------|-------|------|------|-----------|
| common.serialization | 66 | 66 | 0 | ~1.3s |
| common.config | 90 | 90 | 0 | ~1.3s |
| common.header | 9 | 9 | 0 | ~0.6s |
| common.record | 2679 | 2677 | 2 | ~5.8s |
| clients.admin | 368 | 368 | 0 | ~14.5s |
| clients.producer.internals | 288 | 288 | 0 | ~14.7s |

Note: `common.record` has 2 known failures on JDK25 — `testZstdJniForSkipKeyValueIterator` fails because Mockito 4.x cannot mock anonymous enum subclasses on JDK25 (a HotSpot+Mockito version mismatch, not a CratonVM bug).

Integration tests (KafkaProducerTest, SenderTest, KafkaConsumerTest, etc.) hang without a real Kafka broker — not counted above.

**CratonVM:** All packages fail at discovery phase (Bug 1/2/3 above).

---

## Spring Boot and Tomcat — build prerequisite gap

The source trees at `apps/spring-boot/` (Gradle) and `apps/tomcat/` (Ant) have **not been compiled**. Running their test suites requires:

1. A `java.exe` shim that launches CratonVM (so Gradle/Ant can fork it as the JVM)
2. The source tree compiled under HotSpot first (produce test-classes dirs/JARs)
3. Then replace the JVM with the shim for the test-execution phase

Until that shim exists, the only runnable tests for these apps are the pre-compiled regression-pool probes (which are pass/pass — see pool section in APPS_SUITE_RESULTS.md).

---

## Reproduction

```bash
# Extract kafka-clients test classes
JDK="C:/Program Files/Java/jdk-25"
M2="$HOME/.m2/repository"
mkdir -p /tmp/kafka-tc
cd /tmp/kafka-tc
"$JDK/bin/jar" -xf "$M2/org/apache/kafka/kafka-clients/3.7.1/kafka-clients-3.7.1-test.jar"

# Compile RunTestsList.java (see C:/craton/tmp/runtests/)

# Bug 1: selectPackage triggers newFileSystem
cratonvm --java-home $JDK --Xmx 2g -c "runtests;kafka-clients.jar;junit5.jar" RunTests org.apache.kafka
# => AbstractMethodError: FileSystemProvider.newFileSystem has no Code attribute

# Bug 2: selectClasspathRoots fails for Windows path
cratonvm ... RunTests "C:/craton/tmp/kafka-tc" org.apache.kafka
# => PreconditionViolationException: baseDir must exist

# Bug 3: selectClass triggers method reflection NPE
cratonvm ... RunTestsList org.apache.kafka.common.serialization.SerializationTest
# => NullPointerException: Cannot invoke equals on null in MethodSelector resolution
```
