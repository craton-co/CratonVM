# CratonVM — Path to "runs any Java app" (parallel-agent roadmap)

**Status anchor (2026-04-18):** HelloWorld runs; KC16 `jboss-modules.jar`
no-args and `-version` run; KC16 `-mp <dir> <module>` boots the full
JBoss Modules stack through 10+ class inits and opens module.xml but
fails in the XML parser with a `java.nio.Buffer.checkIndex` AIOOBE
causing `ModuleNotFoundException`. Every item below is sized for a
single isolated background agent.

## Format

Each item is **atomic**: one test, one file-or-three, no cross-item
coordination. Agents can claim any item without reading any other.

| Field | Meaning |
|---|---|
| ID | `R<area>.<N>` — stable handle for PRs |
| Title | Imperative verb phrase, <60 chars |
| Files | Primary files the agent will touch |
| Reproducer | Exact shell command proving "before" |
| Success | Concrete "after" signal |
| Blocks | Other items that can't start until this ships |
| Parallel-safe with | Explicit compatibility notes |

---

## Phase A — Module loading (unblocks Keycloak 16 bootstrap)

### RA.1 — Fix `java.nio.Buffer.checkIndex` limit mismatch — CLAIMED 2026-04-18, DONE

- **Files:** `native-builtins/src/nio_buffer.rs` (or wherever Buffer allocations live), `gc/src/heap.rs` if the limit is stored per-array.
- **Reproducer:** `cd C:/craton/cratonvm && RUST_LOG=cratonvm_vm::runtime::exceptions=debug ./target/release/cratonvm.exe --java-home "C:\Program Files\Java\jdk-25" --Xmx 2g --jar C:/craton/keycloak-16.1.1/jboss-modules.jar -- -mp C:/craton/keycloak-16.1.1/modules org.jboss.as.standalone "-Djboss.home.dir=C:/craton/keycloak-16.1.1" 2>&1 | grep checkIndex` → `origin: AIOOBE {index: 2} class=java/nio/Buffer method=checkIndex pc=11`.
- **Success:** no AIOOBE trace; next failure (if any) is downstream.
- **Parallel-safe with:** everything in Phase A (read-only NIO investigation).
- **Resolution (2026-04-18):** Investigation showed the synthetic CharBuffer
  layout in `phases_late.rs::register_p62_char_buffer` (`position=1, limit=2,
  capacity=3, mark=4`) is already aligned with the real JDK
  `java/nio/Buffer` field declaration order, so slot-2 reads/writes line up.
  The observed AIOOBE was triggered from
  `NormalizerBase$NFKCModeImpl.<clinit>` via `HeapByteBuffer.get(2)` on a
  stale `target/release/cratonvm.exe` that predated other in-tree native
  fixes; a fresh `cargo build --release -p cratonvm-cli` makes the trace
  disappear deterministically (5/5 runs clean) and the KC16 `-mp` failure
  advances to RA.4 (Windows path separator in `module.xml` open). Pinned
  the layout with a regression unit test:
  `phases_late::ra1_char_buffer_roundtrip_tests::char_buffer_capacity10_put5_flip_get5_round_trip`.

### RA.2 — UTF-8 decoding in `InputStreamReader.read([CII)I`

- **Files:** `native-io/src/lib.rs::native_isr_read_chars`.
- **Reproducer:** write a Java main that reads a UTF-8 file containing a multi-byte char (e.g. `©`, 0xC2 0xA9) through `new InputStreamReader(new FileInputStream(f), "UTF-8")` and prints `read[0]`; our current impl returns 0xC2 as char, HotSpot returns 0xA9 (169).
- **Success:** round-trips a 200-byte UTF-8 fixture containing BMP + surrogate-pair characters.
- **Parallel-safe with:** RA.1, RA.3, RA.4.

### RA.3 — `Reader.read(CharBuffer)` native  [DONE 2026-04-18]

- **Files:** `native-io/src/lib.rs`.
- **Reproducer:** grep JDK source — `Reader.read(CharBuffer)` is the default NIO read; MXParser.fillBuf may use it transitively.
- **Success:** registered native routes to `read([CII)I` via `CharBuffer.array()` + offset.
- **Parallel-safe with:** RA.1, RA.2, RA.4.
- **Status:** Native `native_reader_read_charbuffer` registered on
  `java/io/Reader` and `java/io/InputStreamReader`. Allocates a
  scratch `char[]` of `min(remaining, 4096)`, calls
  `this.read([CII)I` then `target.put([CII)Ljava/nio/CharBuffer;` via
  `invoke_virtual` — so it works on any Reader/CharBuffer pair without
  touching NIO Buffer intrinsics. Verified end-to-end with
  `new InputStreamReader(new FileInputStream(...)).read(CharBuffer.allocate(32))`
  reading 11 chars of "hello world". Four unit tests pass
  (`ra3_reader_read_charbuffer_tests::*`).

### RA.4 — Rust-side XML parser for `module.xml`

- **Files:** new `native-builtins/src/jboss_module_xml.rs`, add `quick-xml = "0.31"` to workspace deps if not present.
- **Reproducer:** same as RA.1 — gets past `parseModuleXml` with a valid ModuleSpec.
- **Success:** register a native for `org/jboss/modules/xml/ModuleXmlParser.parseModuleXml(Lorg/jboss/modules/xml/ModuleXmlParser$ResourceRootFactory;Lorg/jboss/modules/ModuleLoader;Ljava/lang/String;Ljava/io/File;Ljava/io/File;)Lorg/jboss/modules/ModuleSpec;` that reads the File via `std::fs`, parses with `quick-xml`, calls back into `ModuleSpec$Builder` via `ctx.invoke` to build the spec. Unit test on the standalone module.xml fixture.
- **Parallel-safe with:** RA.1, RA.2, RA.3. (Alternative/fallback to those three.)

### RA.5 — `ResourceRootFactory.createResourceLoader` for directory roots

- **Files:** `native-builtins/src/` (new or existing `jboss_resource_loader.rs`).
- **Reproducer:** `ModuleXmlParser` invokes `RRF.createResourceLoader(rootDir, loaderPath, loaderName)` to load `.jar` files referenced from `<resource-root path="foo.jar"/>`. Currently: NPE when KC16 tries to add its first resource-root.
- **Success:** registered native opens the target JAR (or directory) as a `ResourceLoader` that can enumerate entries and return `ClassSpec`s.
- **Parallel-safe with:** RA.1..4.

### RA.6 — Backport `java/io/File.toPath` → real `Path`

- **Files:** `native-io/src/`.
- **Reproducer:** `new File("x").toPath()` should return a `Path` whose `.toString()` round-trips. Current: synthetic Path missing methods.
- **Success:** toPath → Paths.get → NIO Path that supports `.resolve`, `.getParent`, `.normalize`, `.toAbsolutePath`.
- **Parallel-safe with:** RA.1..5.

### RA.7 — `java.util.jar.JarFile` real-mode natives

- **Files:** `native-io/src/zip_real_jar.rs` (new).
- **Reproducer:** `new JarFile(new File("kc.jar"))` then `getManifest()` — currently throws or returns null.
- **Success:** opens JAR via our existing `zip` flate2 machinery (`native-builtins/src/zip_real.rs`), exposes `entries()`, `getEntry(String)`, `getInputStream(ZipEntry)`.
- **Parallel-safe with:** all RA.*.

### RA.8 — `ServiceLoader.load(Class, ClassLoader)` scans META-INF/services

- **Files:** `native-builtins/src/` (new `service_loader.rs` or extend existing).
- **Reproducer:** `ServiceLoader.load(CharsetProvider.class).iterator()` should enumerate providers declared in `../../../apps/META-INF/services/java.nio.charset.spi.CharsetProvider`. Current: returns empty.
- **Success:** walks the classpath for `../../../apps/META-INF/services/<interface>` files, loads each listed class.
- **Parallel-safe with:** all RA.*.

---

## Phase B — Charset + I/O completeness (unblocks Hibernate/XML/JSON)

### RB.1 — `Charset.forName("UTF-8")` returns a usable Charset

- **Files:** `native-builtins/src/charset.rs` (new or existing).
- **Reproducer:** `Charset.forName("UTF-8").newDecoder().decode(ByteBuffer.wrap(...))` currently returns empty CharBuffer.
- **Success:** UTF-8 + US-ASCII + ISO-8859-1 + UTF-16LE/BE each round-trip a 256-byte fixture.
- **Parallel-safe with:** RB.2..5.

### RB.2 — `Charset.defaultCharset()` returns UTF-8 on JDK 18+ hosts

- **Files:** `native-builtins/src/charset.rs`.
- **Reproducer:** current returns null / hangs.
- **Success:** returns the Charset instance produced by RB.1.
- **Parallel-safe with:** RB.1 (depends), RB.3..5.

### RB.3 — `sun.nio.cs.StreamDecoder` real-mode shim

- **Files:** `native-io/src/stream_decoder.rs` (new).
- **Reproducer:** even with RA.2 landed, some JDK code instantiates `StreamDecoder` directly (e.g. `Files.newBufferedReader(path)`).
- **Success:** `StreamDecoder.forInputStreamReader(InputStream, Object, Charset)` returns a Reader our ISR native can read through.
- **Parallel-safe with:** RA.2.

### RB.4 — `sun.nio.cs.StreamEncoder` real-mode shim

- **Files:** `native-io/src/stream_encoder.rs` (new).
- **Reproducer:** `OutputStreamWriter.write(String)` — currently silent drops.
- **Success:** writes to underlying OutputStream via `write([BII)` invokevirtual.
- **Parallel-safe with:** RB.3.

### RB.5 — `BufferedReader.readLine()` UTF-8 aware

- **Files:** `native-io/src/lib.rs` (existing `native_br_read_line`).
- **Reproducer:** `new BufferedReader(new InputStreamReader(fis, "UTF-8")).readLine()` on a multi-line UTF-8 fixture.
- **Success:** returns each line with correct chars; `\r\n`, `\r`, `\n` all terminate.
- **Parallel-safe with:** RB.1..4.

### RB.6 — `java.io.DataInputStream.readUTF` (modified-UTF-8)

- **Files:** `native-io/src/lib.rs`.
- **Reproducer:** serialization round-trip `ObjectOutputStream.writeUTF(s)` → `ObjectInputStream.readUTF()`.
- **Success:** 20-char fixture with surrogate-pair and null-byte reversibly round-trips.
- **Parallel-safe with:** RB.1..5.

### RB.7 — `java.io.PrintWriter.println` without BOM

- **Files:** `native-io/src/lib.rs`.
- **Reproducer:** `pw.println("x")` writes `78 0a` (or `78 0d 0a` on Win line sep), no leading `\uFEFF`.
- **Success:** hexdump matches.
- **Parallel-safe with:** RB.1..6.

### RB.8 — `Files.newBufferedReader` / `newBufferedWriter` roundtrip

- **Files:** `native-io/src/nio_files.rs` (existing or new).
- **Reproducer:** write then read a file via the above APIs, expect content equal.
- **Success:** passes the round-trip assertion.
- **Parallel-safe with:** RB.1..7.

---

## Phase C — Reflection hardening (unblocks CDI, Jackson, Hibernate)

### RC.1 — `Class.getDeclaredFields` returns real Field[] with correct modifiers

- **Files:** `native-builtins/src/lang_class.rs`.
- **Reproducer:** `A.class.getDeclaredFields()` where A has `private static final int x = 1` — our current returns length-0 or wrong modifiers.
- **Success:** fixture with 5 fields (static/instance × private/public × primitive/reference) — assert name, type, modifiers on each.
- **Parallel-safe with:** RC.2..8.

### RC.2 — `Class.getDeclaredMethods` with signatures

- **Files:** same as RC.1.
- **Reproducer:** `A.class.getDeclaredMethods()` with overloaded methods and generic types.
- **Success:** returns each declared method with correct name, params, return type, modifiers. Does NOT include inherited methods.
- **Parallel-safe with:** RC.1 (same file but different section).

### RC.3 — `Class.getConstructors` / `getDeclaredConstructors`

- **Files:** same.
- **Reproducer:** fixture class with 3 constructors (no-arg, single-arg, varargs).
- **Success:** all 3 returned with correct paramTypes and modifiers.
- **Parallel-safe with:** RC.1, RC.2.

### RC.4 — `Field.set` / `Field.get` via Unsafe offsets

- **Files:** `native-builtins/src/lang_reflect_field.rs`.
- **Reproducer:** `f.setAccessible(true); f.set(obj, 42); f.get(obj)` should round-trip.
- **Success:** round-trip test on primitive int, long, Object, boolean, double.
- **Parallel-safe with:** RC.1..3, RC.5..8.

### RC.5 — `Method.invoke(Object, Object...)` with boxing

- **Files:** `native-builtins/src/lang_reflect_method.rs`.
- **Reproducer:** `m.invoke(obj, Integer.valueOf(42))` on `void m(int)` should unbox.
- **Success:** passes boxing/unboxing matrix for all 8 primitives.
- **Parallel-safe with:** RC.1..4, RC.6..8.

### RC.6 — `Constructor.newInstance(Object[])` handles var-args

- **Files:** `native-builtins/src/lang_reflect_ctor.rs`.
- **Reproducer:** `ctor.newInstance(new Object[]{42, "hi"})` on 2-arg constructor.
- **Success:** matches HotSpot behavior including IllegalArgumentException on type mismatch.
- **Parallel-safe with:** RC.1..5, RC.7..8.

### RC.7 — `AccessibleObject.setAccessible(true)` bypasses JPMS checks

- **Files:** `native-builtins/src/lang_reflect.rs`.
- **Reproducer:** `f.setAccessible(true)` on a field of `java.util.HashMap` must not throw `InaccessibleObjectException` in our bootstrap path.
- **Success:** JBoss Modules reflectively sets accessibility on internal fields without error.
- **Parallel-safe with:** RC.1..6, RC.8.

### RC.8 — `MethodHandles.Lookup.findVirtual` / `findStatic` / `findSpecial`

- **Files:** `native-builtins/src/lang_invoke.rs`.
- **Reproducer:** `MethodHandles.lookup().findStatic(Integer.class, "parseInt", MethodType.methodType(int.class, String.class)).invoke("42")` returns 42.
- **Success:** matrix test for static/virtual/special × primitive/object × varargs.
- **Parallel-safe with:** RC.1..7.

---

## Phase D — Concurrency primitives (unblocks Jetty/Tomcat, ExecutorService)

### RD.1 — `j.u.c.atomic.AtomicInteger` / `AtomicLong` CAS

- **Files:** `native-builtins/src/` (existing concurrent).
- **Reproducer:** `AtomicInteger ai = new AtomicInteger(0); ai.compareAndSet(0, 1);` returns true, `ai.get() == 1`.
- **Success:** 8-thread contention test where 100k CAS operations increment to exactly 100k × 8 = 800k.
- **Parallel-safe with:** RD.2..10.

### RD.2 — `AtomicReference.compareAndSet`

- **Files:** same.
- **Reproducer:** `AtomicReference<String> ar = new AtomicReference<>("a"); ar.compareAndSet("a", "b");` returns true.
- **Success:** reference-identity CAS (not equals-based).
- **Parallel-safe with:** RD.1, RD.3..10.

### RD.3 — `ConcurrentHashMap.put` / `get` / `remove` 8-thread stress

- **Files:** same.
- **Reproducer:** 8 threads × 10k put/get/remove on shared CHM; no lost updates, no NPE.
- **Success:** post-run size matches expected invariants.
- **Parallel-safe with:** RD.1..2, RD.4..10.

### RD.4 — `ReentrantLock.lock()` / `unlock()` reentrance counter

- **Files:** same.
- **Reproducer:** `lock.lock(); lock.lock(); lock.unlock(); lock.isHeldByCurrentThread()` should return true; second unlock releases.
- **Success:** reentrance depth tracked per-thread.
- **Parallel-safe with:** RD.1..3, RD.5..10.

### RD.5 — `Condition.await` / `signal` timed wait

- **Files:** same.
- **Reproducer:** producer-consumer with ArrayBlockingQueue-like semantics.
- **Success:** 2-thread producer/consumer passes 1000 items within 1s.
- **Parallel-safe with:** RD.1..4, RD.6..10.

### RD.6 — `ThreadPoolExecutor.submit(Callable)` returns Future

- **Files:** same.
- **Reproducer:** `ExecutorService es = Executors.newFixedThreadPool(4); Future<Integer> f = es.submit(() -> 42); f.get()` returns 42.
- **Success:** 100 tasks complete, results match.
- **Parallel-safe with:** RD.1..5, RD.7..10.

### RD.7 — `ForkJoinPool.commonPool()` with work-stealing

- **Files:** same.
- **Reproducer:** parallel stream `IntStream.range(0, 1000).parallel().sum()`.
- **Success:** returns 499500.
- **Parallel-safe with:** RD.1..6, RD.8..10.

### RD.8 — `CompletableFuture.supplyAsync().thenApply().get()`

- **Files:** same.
- **Reproducer:** chain 4 stages, final `.get()` returns correctly.
- **Success:** fixture with exception propagation also works (`.exceptionally` catches).
- **Parallel-safe with:** RD.1..7, RD.9..10.

### RD.9 — `Thread.sleep(long, int)` nanosecond precision

- **Files:** `native-builtins/src/lang_thread.rs`.
- **Reproducer:** `long t = System.nanoTime(); Thread.sleep(0, 500_000); long e = System.nanoTime();` should show `e - t >= 500_000` (within OS scheduler slack).
- **Success:** test passes with 10 trials.
- **Parallel-safe with:** RD.1..8, RD.10.

### RD.10 — `Thread.join()` / `join(long)` timeout semantics

- **Files:** same.
- **Reproducer:** spawn thread that sleeps 1s, `t.join(100)` returns after ~100ms with `t.isAlive() == true`.
- **Success:** test passes deterministically.
- **Parallel-safe with:** RD.1..9.

---

## Phase E — Networking (unblocks HTTP clients, servlet containers)

### RE.1 — `java.net.Socket.connect(SocketAddress, int)` real TCP

- **Files:** `native-builtins/src/net_socket.rs`.
- **Reproducer:** `Socket s = new Socket(); s.connect(new InetSocketAddress("127.0.0.1", 8080), 1000);` against a simple server started in the test.
- **Success:** end-to-end bytes round-trip through the socket.
- **Parallel-safe with:** RE.2..10.

### RE.2 — `ServerSocket.accept()` with timeout

- **Files:** same.
- **Reproducer:** `ServerSocket ss = new ServerSocket(0); int port = ss.getLocalPort(); /* connect from client */; Socket s = ss.accept();` round-trips.
- **Success:** test passes.
- **Parallel-safe with:** RE.1.

### RE.3 — `InetAddress.getByName(String)` DNS resolution

- **Files:** `native-builtins/src/net_inet.rs`.
- **Reproducer:** `InetAddress.getByName("localhost").getHostAddress()` returns `127.0.0.1`.
- **Success:** resolves both IPv4 and IPv6 literals and localhost.
- **Parallel-safe with:** RE.1..2, RE.4..10.

### RE.4 — `URL.openConnection().getInputStream()` for HTTP

- **Files:** `native-builtins/src/net_http.rs` (new).
- **Reproducer:** `new URL("http://localhost:PORT/").openStream()` reads a known body from a test server.
- **Success:** content-length match on fixture.
- **Parallel-safe with:** RE.1..3, RE.5..10.

### RE.5 — `HttpClient.send(HttpRequest, BodyHandler)` (JDK 11+)

- **Files:** same.
- **Reproducer:** `HttpClient.newHttpClient().send(HttpRequest.newBuilder(URI.create("http://localhost:PORT/")).build(), BodyHandlers.ofString())` returns the body.
- **Success:** GET, POST, 30x redirect, 404 all pass.
- **Parallel-safe with:** RE.1..4, RE.6..10.

### RE.6 — `javax.net.ssl.SSLContext.getInstance("TLS")` stub → rustls

- **Files:** `native-builtins/src/tls.rs`.
- **Reproducer:** `HttpsURLConnection.getInputStream()` against `https://localhost:PORT/` with a self-signed cert installed.
- **Success:** handshake + read body.
- **Parallel-safe with:** RE.1..5, RE.7..10. Depends on rustls workspace dep (probably present).

### RE.7 — `DatagramSocket.send` / `receive`

- **Files:** `native-builtins/src/net_datagram.rs`.
- **Reproducer:** UDP loopback ping-pong.
- **Success:** echoes back 1000-byte payload.
- **Parallel-safe with:** RE.1..6, RE.8..10.

### RE.8 — `NetworkInterface.getNetworkInterfaces()`

- **Files:** `native-builtins/src/net_interface.rs`.
- **Reproducer:** enumerates at least one interface with a valid MAC.
- **Success:** iterator non-empty, each interface has name + 0+ addresses.
- **Parallel-safe with:** RE.1..7, RE.9..10.

### RE.9 — `Selector.select(long)` NIO

- **Files:** `native-builtins/src/nio_selector.rs`.
- **Reproducer:** non-blocking server using `Selector` accepts 10 connections.
- **Success:** passes Netty-style echo test.
- **Parallel-safe with:** RE.1..8, RE.10.

### RE.10 — `HttpServer.create(InetSocketAddress, backlog)` (com.sun.net.httpserver)

- **Files:** `native-builtins/src/net_httpserver.rs` (or existing).
- **Reproducer:** spin up a `HttpServer`, register a handler, GET from `HttpClient`.
- **Success:** round-trip.
- **Parallel-safe with:** RE.1..9.

---

## Phase F — Cryptography (unblocks HTTPS, JWT, password hashing)

### RF.1 — `MessageDigest.getInstance("SHA-256")` real digest

- **Files:** `native-builtins/src/crypto_digest.rs`.
- **Reproducer:** `MessageDigest.getInstance("SHA-256").digest("hello".getBytes())` matches the 32-byte known hash.
- **Success:** test vectors for MD5, SHA-1, SHA-256, SHA-512 all pass.
- **Parallel-safe with:** RF.2..10.

### RF.2 — `Mac.getInstance("HmacSHA256")` HMAC

- **Files:** `native-builtins/src/crypto_mac.rs`.
- **Reproducer:** RFC 4231 test vectors.
- **Success:** HMAC-SHA256, HMAC-SHA1, HMAC-MD5 all pass.
- **Parallel-safe with:** RF.1, RF.3..10.

### RF.3 — `Cipher.getInstance("AES/CBC/PKCS5Padding")` + `AES/GCM/NoPadding`

- **Files:** `native-builtins/src/crypto_cipher.rs`.
- **Reproducer:** NIST test vectors for AES-128-CBC and AES-256-GCM.
- **Success:** encrypt/decrypt round-trip matches.
- **Parallel-safe with:** RF.1..2, RF.4..10.

### RF.4 — `KeyGenerator.getInstance("AES")` + `SecretKeyFactory`

- **Files:** same.
- **Reproducer:** generate 256-bit AES key, use it with RF.3 cipher.
- **Success:** round-trip.
- **Parallel-safe with:** RF.3.

### RF.5 — `Signature.getInstance("SHA256withRSA")`

- **Files:** `native-builtins/src/crypto_signature.rs`.
- **Reproducer:** sign and verify a 200-byte payload with a generated 2048-bit RSA key.
- **Success:** verify returns true; tamper → false.
- **Parallel-safe with:** RF.1..4, RF.6..10.

### RF.6 — `KeyPairGenerator.getInstance("RSA")` / `EC`

- **Files:** `native-builtins/src/crypto_keypair.rs`.
- **Reproducer:** `kpg.initialize(2048); kpg.generateKeyPair();`.
- **Success:** keys pair-verify via RF.5.
- **Parallel-safe with:** RF.5.

### RF.7 — `SecureRandom.getInstance("NativePRNG")` uses OS CSPRNG

- **Files:** `native-builtins/src/crypto_random.rs`.
- **Reproducer:** 1M random ints, chi-squared test passes uniformity.
- **Success:** passes NIST SP 800-22 subset (frequency, runs, longest-run).
- **Parallel-safe with:** RF.1..6, RF.8..10.

### RF.8 — `KeyStore.getInstance("PKCS12")` load/save

- **Files:** `native-builtins/src/crypto_keystore.rs`.
- **Reproducer:** load `/tmp/test.p12` (keytool-generated), read alias, extract cert.
- **Success:** cert matches expected.
- **Parallel-safe with:** RF.1..7, RF.9..10.

### RF.9 — `CertificateFactory.generateCertificate(InputStream)`

- **Files:** `native-builtins/src/crypto_cert.rs`.
- **Reproducer:** parse a PEM-encoded X.509 cert, extract subject DN.
- **Success:** `cert.getSubjectX500Principal().getName()` matches fixture.
- **Parallel-safe with:** RF.1..8, RF.10.

### RF.10 — `javax.net.ssl.TrustManagerFactory.init(KeyStore)`

- **Files:** `native-builtins/src/tls.rs`.
- **Reproducer:** load the JDK cacerts, build a TrustManager, use in SSLContext.
- **Success:** verifies a real public cert (e.g. `letsencrypt-x3`).
- **Parallel-safe with:** RF.1..9.

---

## Phase G — JIT / Interpreter correctness (latent miscompiles)

### RG.1 — JIT `invokedynamic makeConcatWithConstants` emit LDC

- **Files:** `jit/src/x64.rs`.
- **Reproducer:** Java `"a" + 1 + "b"` compiled to invokedynamic; JIT must not drop the constant chunks.
- **Success:** JIT output matches interpreter byte-for-byte on a 50-concat fixture.
- **Parallel-safe with:** RG.2..12.

### RG.2 — JIT FP NaN comparison canonicalization

- **Files:** `jit/src/x64.rs`.
- **Reproducer:** `Double.NaN == Double.NaN` must be false; `Double.compare(NaN, 1.0) > 0`.
- **Success:** FP comparison matrix with NaN, +0, -0, +inf, -inf all matches HotSpot.
- **Parallel-safe with:** RG.1, RG.3..12.

### RG.3 — JIT `lcmp` / `fcmpg` / `fcmpl` / `dcmpg` / `dcmpl` semantics

- **Files:** same.
- **Reproducer:** `Long.compare`, `Float.compare` roundtrip matrix.
- **Success:** 64-case comparison matrix passes.
- **Parallel-safe with:** RG.2.

### RG.4 — JIT `tableswitch` / `lookupswitch` dense & sparse

- **Files:** same.
- **Reproducer:** fixture with 3 switch statements: dense 1..100, sparse {1, 1000, 1000000}, default-only.
- **Success:** all match interpreter.
- **Parallel-safe with:** RG.1..3, RG.5..12.

### RG.5 — JIT exception handlers (try/catch) through JIT code

- **Files:** `jit/src/x64.rs`, `vm/src/jit/helpers.rs`.
- **Reproducer:** JIT-compiled method throws AE, caught by JIT-compiled frame two levels up.
- **Success:** stack unwinds correctly, local vars restored, caught value correct.
- **Parallel-safe with:** RG.1..4, RG.6..12.

### RG.6 — JIT synchronized-method entry/exit

- **Files:** same.
- **Reproducer:** 100 threads × `synchronized` method increments shared counter.
- **Success:** final count matches expected (no lost updates, no deadlock).
- **Parallel-safe with:** RG.1..5, RG.7..12.

### RG.7 — JIT `arraycopy` intrinsic for all primitive types

- **Files:** same.
- **Reproducer:** 100k `System.arraycopy` calls across 8 primitive types.
- **Success:** each type's intrinsic writes correct bytes, respects overlapping-array semantics.
- **Parallel-safe with:** RG.6.

### RG.8 — JIT GC safepoint at loop backedges

- **Files:** `jit/src/x64.rs`.
- **Reproducer:** long-running JIT'd loop during concurrent GC cycle.
- **Success:** GC pauses and resumes the loop without data corruption.
- **Parallel-safe with:** RG.1..7, RG.9..12.

### RG.9 — JIT tier-up from Interp→JIT mid-loop

- **Files:** `vm/src/jit/tiered.rs`.
- **Reproducer:** method runs for 10k iterations in interpreter, gets JIT-compiled, continues in JIT.
- **Success:** local state (all slots) identical pre/post transition.
- **Parallel-safe with:** RG.1..8, RG.10..12.

### RG.10 — Interpreter `putfield` / `putstatic` with reference-int coercion correctness

- **Files:** `vm/src/runtime/interpreter.rs` (carefully — prior fragile changes).
- **Reproducer:** fixture class with `volatile int` field set/get 100k times from 4 threads.
- **Success:** no lost updates, volatility respected.
- **Parallel-safe with:** RG.1..9. Careful overlap with interpreter-touching items.

### RG.11 — Interpreter wide-index (`wide`) opcode prefix

- **Files:** `vm/src/runtime/interpreter.rs`.
- **Reproducer:** method with 400 locals; iload 300 must dispatch through `wide iload_w`.
- **Success:** passes.
- **Parallel-safe with:** RG.1..10, RG.12.

### RG.12 — Interpreter `invokedynamic` for lambda capture

- **Files:** `vm/src/runtime/invokedynamic.rs`.
- **Reproducer:** `Runnable r = () -> System.out.println("hi"); r.run();` must print "hi".
- **Success:** lambda creation, capture of local via indy handle, invocation.
- **Parallel-safe with:** RG.1..11.

---

## Phase H — GC / memory robustness

### RH.1 — Generational GC promotion barrier under 1GB pressure

- **Files:** `gc/src/gen_heap.rs`.
- **Reproducer:** allocate 2GB of short-lived objects; young→old promotion must not corrupt.
- **Success:** no stale refs, no double-free, GC statistics match allocations.
- **Parallel-safe with:** RH.2..8.

### RH.2 — Reference queue delivery for WeakReference/SoftReference

- **Files:** `gc/src/weak.rs`.
- **Reproducer:** `WeakReference<byte[]> wr = new WeakReference<>(new byte[1024*1024], queue); System.gc(); queue.poll()` returns the ref.
- **Success:** test passes within 1s.
- **Parallel-safe with:** RH.1, RH.3..8.

### RH.3 — `PhantomReference` + Cleaner API

- **Files:** `gc/src/phantom.rs` + `native-builtins/src/cleaner.rs`.
- **Reproducer:** resource cleaned via `Cleaner.register(obj, cleanup)` after `obj` unreachable.
- **Success:** cleanup runs within 2 GC cycles.
- **Parallel-safe with:** RH.1..2, RH.4..8.

### RH.4 — Finalize queue ordering

- **Files:** `gc/src/finalizer.rs`.
- **Reproducer:** 10 objects with finalizers, assert LIFO order. (Java allows any order; just don't crash.)
- **Success:** all 10 finalizers run without NPE/panic.
- **Parallel-safe with:** RH.1..3, RH.5..8.

### RH.5 — `ByteBuffer.allocateDirect` via Unsafe.allocateMemory

- **Files:** `native-builtins/src/nio_buffer_direct.rs`.
- **Reproducer:** 1MB direct buffer, put/get round-trip, GC after dereference frees underlying memory.
- **Success:** no memory leak after 1000 allocate/release cycles (measured via process RSS).
- **Parallel-safe with:** RH.1..4, RH.6..8.

### RH.6 — `Unsafe.compareAndSwap{Int,Long,Object}` volatile semantics

- **Files:** `native-builtins/src/unsafe.rs`.
- **Reproducer:** 8-thread `AtomicLong` stress test (2M CAS operations total).
- **Success:** final value correct, no torn reads.
- **Parallel-safe with:** RH.1..5, RH.7..8.

### RH.7 — GC root scan includes JIT stack frames (OopMaps)

- **Files:** `jit/src/oopmap.rs`, `vm/src/memory/roots.rs`.
- **Reproducer:** JIT'd method holds reference across an allocation that triggers GC.
- **Success:** reference survives GC; no UAF on subsequent deref.
- **Parallel-safe with:** RH.1..6, RH.8.

### RH.8 — G1 region eviction policy

- **Files:** `gc/src/g1.rs`.
- **Reproducer:** allocation pattern that fragments old gen; G1 should evacuate high-garbage regions first.
- **Success:** throughput test shows <10% slowdown vs HotSpot G1 equivalent.
- **Parallel-safe with:** RH.1..7.

---

## Phase I — Smoke tests per app family

Each of these is a single agent task that runs a real app end-to-end.
Fail/pass is binary. When it passes the agent opens follow-up items
for any specific native gaps encountered.

### RI.1 — SPECjvm2008 compiler.compiler benchmark runs

### RI.2 — DaCapo avrora benchmark runs

### RI.3 — DaCapo jython benchmark runs

### RI.4 — Apache Commons Lang test suite (unit tests, no server)

### RI.5 — Jackson databind round-trip JSON test

### RI.6 — SLF4J + Logback: log a message to a file

### RI.7 — Tomcat 10 Embed: respond to GET /

### RI.8 — Jetty 11 Embedded: respond to GET /

### RI.9 — Spring Boot 3.2 "Hello World" starter app

### RI.10 — Hibernate 6 + H2: insert then select one row

### RI.11 — Netty 4 echo server + client loopback

### RI.12 — Kotlin hello-world (kotlinc-compiled .class)

### RI.13 — Scala 3 hello-world

### RI.14 — Groovy 4 script

### RI.15 — Quarkus 3 fast-jar (hot-path of KC26)

### RI.16 — Keycloak 16 `-mp` boot to admin console

### RI.17 — Keycloak 26 Quarkus boot

Each RI.N is a SINGLE AGENT task whose prompt is:
> Run `<exact reproducer command>`. If it doesn't end in the documented
> success signal, triage the first error, either fix it inline (if a
> registered-native or small bytecode issue) OR add new items RX.N for
> the discovered gaps. Pass criterion: documented success signal
> observed. Max 1 fix per agent; hand-off further fixes to new items.

---

## Phase K — App-triage discoveries (from parallel agent runs)

### RApp.1 — Fix verifier frame-mismatch in `StringUtils.indexOfAny` (Commons Lang 3.14.0)

- **Files:** `vm/src/verifier/` (StackMapTable frame assignability logic), possibly `vm/src/runtime/interpreter.rs` typing helpers (read-only for diagnosis).
- **Reproducer:** from `C:\craton\cratonvm`:
  ```
  ./target/release/cratonvm.exe --java-home "C:\Program Files\Java\jdk-25" --Xmx 512m --classpath "apps/commons-lang3;apps/commons-lang3/commons-lang3.jar" LangTest
  ```
  Observed stderr:
  ```
  Error in thread "main" linkage error: verification error in org/apache/commons/lang3/StringUtils.indexOfAny: frame mismatch at bytecode offset 70: current frame is not assignable to declared StackMapTable frame
  ```
- **Success:** `LangTest` boots past class init of `StringUtils`; all 8 assertions print `PASS`.
- **Notes:** `commons-lang3-3.14.0` is built for Java 8 and has StackMapTable frames that modern verifiers accept. The rejected frame at offset 70 in `indexOfAny(CharSequence, char...)` is a common pattern where the verifier must merge a CharSequence local with either a String or null. Check whether our verifier treats `uninitializedThis`, `top`, or null assignability too strictly, and whether reference subtype widening uses loaded class-hierarchy info. Inspect the actual class file with `javap -v -p -c` on `org.apache.commons.lang3.StringUtils` to get the exact declared frame at offset 70 vs. the current frame we compute.
- **Parallel-safe with:** everything (verifier-only; isolated from codegen).
- **Blocks:** any future `RApp.*` item that wants to exercise a library built with modern `javac` StackMapTable output.

## Phase J — Housekeeping / removed assertions / final conformance

### RJ.1 — Remove all remaining `[DIAG-*]` / `[TRACE-*]` / `[SB-TRACE]` prints

- **Files:** grep for `eprintln!(".*DIAG\|TRACE"`) across workspace.
- **Success:** grep returns 0 hits; CI gate enforces it.
- **Parallel-safe with:** everything (read-only until final commit).

### RJ.2 — `TodoWrite` reminder fires only when a real todo is active

- **Files:** n/a — documentation-only.

### RJ.3 — CI runs RI.1..RI.17 on every merge

- **Files:** `.github/workflows/jvm-smoke.yml` (new).

### RJ.4 — `cargo doc --workspace` warnings → 0

- **Files:** crate-level attributes across workspace.
- **Parallel-safe with:** everything.

### RJ.5 — JCK section compliance matrix

- **Files:** `docs/jck-compliance.md` (new).
- **Success:** each JCK section has a compliance % and a bug-per-section tally.

---

## How to claim an item

1. Pick any item whose "Blocks" list is empty OR already-shipped.
2. Spawn an agent with the prompt template:
   > "Claim RA.1. Files to touch: `...`. Reproducer: `...`. Success:
   > `...`. Constraints: don't modify `vm/src/runtime/value_stack.rs`,
   > `vm/src/runtime/interpreter.rs`, or `native-builtins/src/phases_late.rs`
   > unless strictly necessary. No emojis. Keep report under 400 words."
3. When it reports pass, tick the checkbox in this file.
4. Check if any "Blocks" list entry now has its dependency shipped —
   those items are newly eligible.

## Global constraints for every agent

- No emojis in source or docs.
- Do not touch `vm/src/runtime/value_stack.rs`, the Getfield/Putfield
  blocks in `vm/src/runtime/interpreter.rs`, or
  `native-builtins/src/phases_late.rs::register_phase71_natives` unless
  the item's Files explicitly lists them.
- Every new native registration goes in `register_essential_natives`
  (unconditional) unless it's strictly a synthetic-mode override, in
  which case it goes inside `register_synthetic_overrides` behind
  `#[cfg(feature = "synthetic-jdk")]`.
- Every new Rust module needs at least one `#[cfg(test)] mod tests`
  with one assertion.
- Reproducer shell command must work from `C:\craton\cratonvm`
  on Windows.

### RSLF4J.1 — Fix `ClassLoader.getSystemClassLoader().getResources(...)` for classpath JARs

| Field | Value |
|---|---|
| ID | RSLF4J.1 |
| Title | System classloader `getResources` returns empty for META-INF/services inside classpath JARs |
| Files | `native-builtins/src/classloader.rs` (cl_get_resources); investigate `URLClassPath` synthesis; possibly `vm/src/runtime/class_loader_impl.rs` |
| Reproducer | `./target/release/cratonvm.exe --java-home "C:\Program Files\Java\jdk-25" --Xmx 512m --classpath "apps/slf4j;apps/slf4j/slf4j-api.jar;apps/slf4j/slf4j-simple.jar" LogTest` — emits `SLF4J(W): No SLF4J providers were found.` and silently drops every log call (exit 0, zero lines on stderr). `ResTest.java` (see `apps/slf4j/`) confirms `getSystemClassLoader().getResources("META-INF/services/org.slf4j.spi.SLF4JServiceProvider")` returns 0 entries even though `slf4j-simple.jar` on the classpath contains that exact entry. |
| Root cause | `<clinit>` of `jdk/internal/loader/URLClassPath` is swallowed as `<clinit> failed with non-critical exception — marking as initialized error=java/lang/NullPointerException`, so `URLClassPath` is broken; `ClassLoader.getResources` (a real Java method, not a native) delegates to it and returns empty. Our `cl_get_resources` native override is never consulted for the real-JDK `ClassLoader.getResources` method because it is concrete, not abstract, and not in the ByteArrayInputStream allowlist in `vm_exec.rs` (line ~2832). |
| Notes | Two possible fixes: (a) fix `URLClassPath` clinit NPE so real classloader works; (b) widen the native-override allowlist in `vm_exec.rs` so that concrete methods can be intercepted when a native is explicitly registered (tighter but broader behavior change). Existing `native-builtins/src/service_loader.rs::register_service_loader_natives` is dead code — `ServiceLoader.load` is not native in JDK 25 and is never registered from `lib.rs`. Existing `register_slf4j_natives` is also dead code for the same reason (`LoggerFactory.getLogger` is not native). |
| Success | SLF4J-simple prints `[main] INFO LogTest - hello from slf4j-simple` and sibling WARN/ERROR lines to stderr on the reproducer above. |
| Blocks | Anything using ServiceLoader from classpath JARs: JDBC drivers, logging backends, Jackson modules, Charset providers loaded from user JARs. |
| Parallel-safe with | Phase A–I items; constraint violations: do NOT modify `value_stack.rs`, Getfield/Putfield of `interpreter.rs`, `phases_late::register_phase71_natives`. |

### RVERIF.1 — Fix `<init>` uninitialized-`this` type tracking post-super-call

| Field | Value |
|---|---|
| ID | RVERIF.1 |
| Title | Bytecode verifier: after super `<init>` returns, `aload_0` in subclass `<init>` yields supertype instead of current class |
| Files | `classloading/src/verify_frame.rs`, `classloading/src/verify_insn.rs`, `classloading/src/vtype.rs` (wherever `UninitializedThis` transitions to `ObjectRef` after `invokespecial <init>`) |
| Reproducer | `cd C:/craton/cratonvm/apps/jackson && ../../target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" -c ".;jackson-core.jar;jackson-annotations.jar;jackson-databind.jar" JacksonTest 2>&1` → `Error in thread "main" linkage error: verification error in com/fasterxml/jackson/databind/ObjectMapper.<init>: at bytecode offset 30: expected ObjectRef("com/fasterxml/jackson/databind/ObjectMapper") on stack, found ObjectRef("com/fasterxml/jackson/core/ObjectCodec")`. |
| Root cause | `ObjectMapper extends ObjectCodec`. At offset 30 of `ObjectMapper.<init>`, a `putfield` on a declared-in-ObjectMapper field needs `this` to be `ObjectRef(ObjectMapper)`. Verifier reports `ObjectCodec` on stack instead, suggesting that after `invokespecial ObjectCodec.<init>`, the `UninitializedThis` on the operand stack / in locals is being upgraded to the *called* class (`ObjectCodec`) rather than the declared enclosing class (`ObjectMapper`). Per JVMS §4.10.1.9, after an `invokespecial <init>` that consumes `UninitializedThis`, every `UninitializedThis` in locals/stack must become `ObjectRef(current_class)`, NOT the callee's class. |
| Setup | `mkdir apps/jackson && cd apps/jackson && curl -O https://repo1.maven.org/maven2/com/fasterxml/jackson/core/jackson-{core,annotations,databind}/2.16.1/jackson-{core,annotations,databind}-2.16.1.jar` and create `JacksonTest.java` with `ObjectMapper m = new ObjectMapper(); m.readValue(json, Pet.class); m.writeValueAsString(p);`. |
| Success | `JacksonTest` runs; first error (if any) is downstream of verifier. Existing HelloWorld / JBoss Modules / SLF4J regressions still pass. |
| Blocks | Any real-world library that subclasses a class using an `<init>` with field writes post-super (Jackson, many DI frameworks, anything generated by Lombok/ASM). |
| Parallel-safe with | Phase A–I; do NOT modify `vm/src/runtime/value_stack.rs`, Getfield/Putfield of `vm/src/runtime/interpreter.rs`, `phases_late::register_phase71_natives`. |

### RScala.1 — Scala 3 `Predef.println` exits silently; `List.map(_ * 2)` overflows

| Field | Value |
|---|---|
| ID | RScala.1 |
| Title | Scala 3 `Predef.println` silently returns and `List.map(_ * 2)` causes StackOverflowError |
| Files | `vm/src/runtime/invokedynamic.rs`, `vm/src/runtime/interpreter.rs` (lambda dispatch), likely `scala/Console$`/`scala/Predef$` initialization paths. Do NOT modify `vm/src/runtime/value_stack.rs`, Getfield/Putfield of `vm/src/runtime/interpreter.rs`, `phases_late::register_phase71_natives`. |
| Reproducer | `cd C:/craton/cratonvm/apps/scala && ./target/release/cratonvm.exe --classpath "out;scala3-library_3-3.4.0.jar;scala-library-2.13.12.jar" run 2>stderr.log` with `hello.scala` compiled via HotSpot's `dotty.tools.dotc.Main`. Current output: silent exit 1 with `Exception in thread "main" java/lang/StackOverflowError`. HotSpot prints `sum=15 doubled=[2,4,6,8,10]` then `OK`. |
| Two distinct gaps | (a) `scala.Predef.println("x")` returns with no output and no stderr (main thread silently continues as if the call succeeded, but nothing prints); `System.out.println("x")` works fine. Occurs even in the minimal `@main def run(): Unit = println("hi")`. Suggests `scala/Predef$.<clinit>` or `scala/Console$.out` initialization silently fails (e.g. `java.io.PrintStream` subclass constructor / `DynamicVariable` / ThreadLocal path), leaving the default-out reference dangling. (b) `xs.map(_ * 2)` originally StackOverflow'd due to mutually-inverse `apply(Object)` ↔ `apply$mcII$sp(int)` default methods on `scala.Function1` and `scala.runtime.java8.JFunction1$mcII$sp`. A bounded fix landed in `try_lambda_dispatch` (commit of this session) that synthesizes a bridge when method_name is `apply` and the lambda proxy's `functional_interface` is `scala/runtime/java8/JFunctionN$mcXY...$sp` with SAM `apply$mc*$sp`: unbox → invoke SAM impl → box via `Integer.valueOf` etc. Verified: `List(1,2,3).map(_ * 2)` → `mkString(",")` works in isolation (see `apps/scala/m6.scala`). However, the full `hello.scala` still SOs because `hello$package$.run` calls `Predef.println` first (issue a), which presumably recurses somewhere in Scala's lazy-init for `Console.out`. |
| Investigation notes | Compile path: `java -cp <scala3-compiler + scala3-library + scala-library-2.13 + tasty-core + scala-asm + compiler-interface + util-interface + scala3-interfaces + jline-reader/terminal/jna>  dotty.tools.dotc.Main -classpath "scala3-library_3-3.4.0.jar;scala-library-2.13.12.jar" -d out hello.scala`. Requires **scala3-interfaces-3.4.0.jar** explicitly on the compile CP (auto-resolved by sbt/mill in practice). `altMetafactory` with flags=1 (`FLAG_SERIALIZABLE`) and BSM #1 = `scala/runtime/LambdaDeserialize.bootstrap` (only invoked from synthetic `$deserializeLambda$`, not forward path). |
| Partial fix in place | `try_lambda_dispatch` now handles `apply(Object)` calls on Scala specialized function lambdas by unboxing, calling the SAM impl, and boxing back — without traversing the mutually-recursive defaults. |
| Bounded next steps | (1) Trace why `scala/Predef$.println` silently returns on cratonvm (compare with HotSpot under `-verbose:class`): likely `scala/Console$.<clinit>` or `java/io/PrintStream` wrapping fails and the dispatcher returns without pushing a value. (2) Extend RScala.1's fix to `JFunctionN` N=0,2 and other primitive specializations (`mcXY...$sp` with varying return types). |
| Success | `apps/scala/run` (hello.scala) prints `sum=15 doubled=[2,4,6,8,10]\nOK` and exits 0. |
| Parallel-safe with | All Phase A–I; compile-step artifacts are self-contained under `apps/scala/`. |

### RJUnit.1 — `ObjectStreamClass` reflection hits `java.lang.invoke` bootstrap cycle

| Field | Value |
|---|---|
| ID | RJUnit.1 |
| Title | JUnit 4 fails at `ObjectStreamClass.getDeclaredSUID` with `InternalError: null::serialVersionUID cannot be accessed reflectively before java.lang.invoke is initialized` |
| Files | `vm/src/runtime/class_init.rs`, `vm/src/boot/*` (boot class init ordering), `native-builtins/src/java_io_object_stream_class.rs` (missing `ObjectStreamClass.initNative()V`) |
| Reproducer | `cd C:/craton/cratonvm && ./target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" --Xmx 512m --classpath "apps/junit4;apps/junit4/junit.jar;apps/junit4/hamcrest.jar" JUnitTest 2>&1` → `WARN Missing native method java/io/ObjectStreamClass.initNative()V` → `WARN <clinit> failed with non-critical exception — marking as initialized class=java/io/ObjectStreamClass error=java/lang/UnsatisfiedLinkError` → `Exception in thread "main" java/lang/InternalError: null::serialVersionUID cannot be accessed reflectively before java.lang.invoke is initialized`. |
| Root cause | JUnit 4's `JUnitCore` triggers `Description`/`Result` class init, which pulls in `ObjectStreamClass`. `java/io/ObjectStreamClass.<clinit>` calls the unimplemented native `initNative()V`, so it throws `UnsatisfiedLinkError`. The VM swallows it as "non-critical" and marks the class initialized with invalid internal state (null fieldsInfo). Subsequent reflection on `serialVersionUID` sees a null class-name context and throws `InternalError`. Separately, the error message wording suggests JDK 25's `ObjectStreamClass` uses `MethodHandles.Lookup` which requires `java.lang.invoke` initialization ordering we are not enforcing. |
| Fix direction | (a) Implement `java/io/ObjectStreamClass.initNative()V` as a no-op or stub that sets the native statics JDK expects (`hasStaticInitializer` lookup table). (b) Audit "swallow UnsatisfiedLinkError as non-critical" path in `vm_util`: for critical JDK classes (`ObjectStreamClass`, `MethodHandle*`), surface the error or populate state. (c) Verify `java.lang.invoke.MethodHandleNatives` initializes before any serializable bootstrap class. |
| Bounded | Yes — stub a single missing native + possibly tighten class-init error handling. Does not touch restricted files (value_stack.rs, Getfield/Putfield in interpreter.rs, phase71). |
| Success | JUnit 4 `JUnitTest` test reaches `JUnitCore.runClasses` without `InternalError`; next error (if any) is downstream of bootstrap. |
| Blocks | Any app that (de)serializes via `ObjectStreamClass`, including JUnit 4, many logging/config frameworks, anything using default Java serialization. |
| Parallel-safe with | All other items; isolated to native stub + class init. |

## Current anchor counts (refreshed 2026-04-26, Session 94)

- HelloWorld: PASS.
- KC16 no-args: PASS.
- KC16 `-mp <dir> -version`: **REGRESSED** to fail at `Properties.load(Ljava/io/Reader;)V`
  NoSuchMethodError before any module XML is touched — see **RKC16N.1** below.
- KC16 `-mp <dir> org.jboss.as.standalone`: fails at the same RKC16N.1.
  Once #1 ships, the CHM CAS livelock (RKC16N.2) is the next gate, then `[L…;`
  array synthesis (RKC16N.3) for downstream module class loading.
- KC26 Quarkus: also gated on RKC16N.2 (shared CHM livelock) per `kc26-blocker-map.md` #1.

Resolved since previous anchor:
- KC16 `initPhase1` synthetic-stream fallback (was Blocker #3) — runs as
  a non-fatal `WARN`; bootstrap continues.
- 9 static MISSING natives (was Blocker #4) — registered with real bodies.
- Stack-dump watchdog (was Blocker #5) — `[cratonvm] stack-dump watchdog
  armed: will dump + abort after 45s` now first line of every run. SIGTERM
  audit-flush still TBD.

Total items above: ~100, each ≤1 day for an isolated agent. Ship
order: Phase A first (unblocks KC16 RC start), then Phase B+C in
parallel, then D+E+F, then G+H, then I as continuous validation.

---

## Phase KC16N — fresh KC16 boot blockers (2026-04-26, Session 94)

### RKC16N.1 — Implement `java/util/Properties.load(Ljava/io/Reader;)V`

| Field | Value |
|---|---|
| ID | RKC16N.1 |
| Title | Properties.load(Reader) NoSuchMethodError aborts KC16 jboss-modules `Main.<clinit>` |
| Files | `native-builtins/src/properties_sidetable.rs` (extend), `native-builtins/src/lib.rs` (no change unless registration moves) |
| Reproducer | `target/release/cratonvm.exe --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot" --Xmx 2g --jar /tmp/keycloak/keycloak-16.1.1/jboss-modules.jar -- -version` → `linkage error: no such method: java/util/Properties.load(Ljava/io/Reader;)V`. |
| Root cause | `register_properties_sidetable` registers `load(Ljava/io/InputStream;)V` but not `load(Ljava/io/Reader;)V`. JBoss Modules' `Main.<clinit>` reads `version.properties` via an `InputStreamReader` wrapping a `FileInputStream`, then calls `Properties.load(Reader)`. Real JDK has both overloads and shares parsing; ours only has the InputStream variant. |
| Fix | Add `register_properties_sidetable` entry for `load(Ljava/io/Reader;)V`. New native drains the Reader by calling `ctx.invoke_virtual(reader, "read", "([CII)I", &[buf, 0, 4096])` in a loop into a scratch `char[]`, accumulates a `String`, then converts to ISO-8859-1 bytes (chars >255 → `?`) and reuses `parse_properties`. (Long-term cleanup: refactor `parse_properties` to take `&str`; not required for unblock.) |
| Success | Reproducer no longer prints `no such method: …Properties.load(Ljava/io/Reader;)V`. Next failure (if any) is downstream — capture & file as RKC16N.4. |
| Parallel-safe with | RKC16N.2, RKC16N.3, every Phase A–I item. Touches only `properties_sidetable.rs` + a unit test. |
| Constraints | No emojis. Don't touch `value_stack.rs` / Getfield-Putfield in `interpreter.rs` / `phases_late.rs::register_phase71_natives`. |

### RKC16N.2 — Fix CHM `initTable()` CAS livelock (typed-default-slot zero-init)

| Field | Value |
|---|---|
| ID | RKC16N.2 |
| Title | Primitive instance fields read as `Value::Object(None)` post-`alloc_object`, breaking `Unsafe.compareAndSetInt` |
| Files | `gc/src/heap.rs` (`alloc_object`, `read_slot`), `vm/src/vm/vm_exec.rs` (`compare_and_swap_field`, `values_equal_for_cas`). Optional: `gc/src/gen_heap.rs` mirror. |
| Reproducer | A Rust unit test at `gc/tests/`: allocate a class with one declared `int` field, read its slot via `read_slot`, assert `Value::Int(0)`. Currently returns `Value::Object(None)`. End-to-end reproducer: KC16 -version (post-RKC16N.1) and KC26 Quarkus, both deadlock at `ConcurrentHashMap.initTable` PC 0..41. |
| Root cause | `gc::heap::read_slot` is `std::ptr::read::<Value>(ptr)` over zero-initialised bytes (from `alloc_zeroed`), which decodes as the zero-discriminant `Value` variant (likely `Object(None)`) regardless of declared field kind. `Unsafe.compareAndSetInt` reads through `read_slot` and compares against `Value::Int(0)` via `values_equal_for_cas`, never matching. |
| Fix | Pick one of (a) tag every slot at `alloc_object` time with the declared field's default (`Int(0)`, `Long(0)`, `Object(None)`, etc.) using the class's field-descriptor list, OR (b) make `read_slot` carry the declared `BasicType` so it interprets zero bytes as the right discriminant. (a) is simpler and matches HotSpot semantics; (b) is more memory-efficient. |
| Success | New unit test passes; CHM unit test (`new ConcurrentHashMap<>().put("a","b")`) returns within 100 iterations; KC16 -version (after RKC16N.1) progresses past `Main.<clinit>` and starts opening `module.xml`. |
| Parallel-safe with | RKC16N.1, RKC16N.3, all Phase D except RD.1/RD.3 (which would test the fix). Heavily overlaps GC/VM ownership; coordinate with anyone touching `heap.rs`. |
| Constraints | No emojis. Don't touch `value_stack.rs` / Getfield-Putfield in `interpreter.rs` / `phases_late.rs::register_phase71_natives`. |

### RKC16N.3 — Synthesize `[L…;` array classes on demand

| Field | Value |
|---|---|
| ID | RKC16N.3 |
| Title | Array classes resolved by JMOD scan instead of synthesised from element class on demand |
| Files | `classloading/src/class_manager.rs` (or wherever `Class.forName("[Lx;")` lookup lives), `classloading/src/array_class.rs` if present. |
| Reproducer | Unit test: `class_manager.resolve_or_load("[Ljava/util/concurrent/ConcurrentHashMap$Node;")` should return an array `Class` with component = the inner Node class without scanning JMODs. End-to-end: identical to KC26 Blocker #3, observed during KC26 Quarkus boot when ~7 inner-class array types fall back to synthetic stub. |
| Root cause | Array-class lookup reads the classpath/JMOD scan for `[Lx;` filenames, which obviously do not exist. JVMS §5.3.3 specifies array classes are *synthesised* by the bootstrap loader from the resolved component class. |
| Fix | When the requested name starts with `[L` and ends with `;`, strip the wrapper, resolve the component class, then synthesise an array `Class` referencing it (set `array_dimension`, `component_class_id`, `name`) without filesystem I/O. Cache by `name` in the existing class table so subsequent `Class.forName` returns the same instance. |
| Success | New unit test passes; KC26 boot trace no longer logs `synthetic stub fallback` for any `[L…;` name. |
| Parallel-safe with | RKC16N.1, RKC16N.2, all RA/RB/RC/RD/RE/RF items. Single file, no GC interaction. |
| Constraints | No emojis. Don't touch the restricted files. |

### RKC16N.7 — Investigate why `find_method` returns None for JDK 25 `String.equals` / `String.charAt`

| Field | Value |
|---|---|
| ID | RKC16N.7 |
| Title | Real-JDK `java/lang/String` class loaded but `find_method("equals", "(Ljava/lang/Object;)Z")` returns None at vm_exec.rs:4949 |
| Files | `vm/src/vm/vm_exec.rs` (the `None` branch around line 4949 — receives `class_id` of the resolved String class but its method table doesn't expose `equals`); `classloading/src/class_manager.rs` (whatever loads `java/lang/String` from `lib/modules`); `classloading/src/method_table.rs` (or wherever `find_method` lives). |
| Reproducer | After RKC16N.1 + the local recon hack registering `String.equals` in `register_essential_natives` (Session 94 worktree, see `native-builtins/src/lib.rs:316`), KC16 -version still emits `WARN NoSuchMethodError method="java/lang/String.equals(Ljava/lang/Object;)Z"`. The native IS registered; the dispatcher reaches the None arm at vm_exec.rs:4949, and the registry-walk lookup finds nothing because `cls.name` for our resolved String isn't matching, OR the resolved class's `methods` table is empty. |
| Hypothesis | Either (a) JDK 25 `String` from `lib/modules` is loaded as a *partial* class (some methods absent — possibly due to JMOD parser skipping certain attributes); or (b) the dispatcher reaches 4949 with a `class_id` whose `cls.name` is something other than literal `"java/lang/String"` (e.g. an interned variant, or an internal stub). Add `eprintln!("class_name={} method={} {}", cls.name, method_name, descriptor)` at the top of the registry walk to see what's actually being queried. |
| Workaround in place | Session 94 added `String` to the override-allowlist at `vm/src/vm/vm_exec.rs:4900` so the native registry consultation fires *before* the bytecode-resolution fallback. This works around RKC16N.7 by always preferring our native, but the underlying class-load / method-table issue remains. |
| Success | Either: revert the override-allowlist hack and have the natural None-fallback path find the registered native; OR confirm the JDK 25 String loads with all expected methods and the bug was something else. Either way, document the root cause. |
| Parallel-safe with | RKC16N.1 (lands), RKC16N.2, RKC16N.3, RKC16N.5, RKC16N.6 (depends on the resolution here). Single-investigation task, mostly read-only. |
| Constraints | Don't touch restricted files. |

### RKC16N.6 — `String.charAt(I)C` not registered in real-JDK essential natives

| Field | Value |
|---|---|
| ID | RKC16N.6 |
| Title | `java/lang/String.charAt(I)C` NoSuchMethodError during `java/nio/charset/StandardCharsets.<clinit>` in real-JDK mode |
| Files | `native-builtins/src/lib.rs` (move charAt registration into `register_essential_natives`), `native-builtins/src/lang_string.rs` (verify the JDK 25 byte[]+coder layout path of `native_string_char_at`). |
| Reproducer | After RKC16N.1 (or local stub) is in place: `target/release/cratonvm.exe --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot" --Xmx 2g --jar /tmp/keycloak/keycloak-16.1.1/jboss-modules.jar -- -version` → `WARN NoSuchMethodError method="java/lang/String.charAt(I)C"` from `java/nio/charset/StandardCharsets.<clinit>`. |
| Root cause | `String.charAt(I)C` is registered at `native-builtins/src/lib.rs:4261` inside `register_synthetic_overrides`. Real-JDK mode never calls that function (see lib.rs:2694 comment). The JDK 25 `String` class loaded from `lib/modules` *should* expose the bytecode method, but our class loader / dispatch fails to find it for this resolution path. Possibly an entry-point fast-path that consults the native registry first and short-circuits to NoSuchMethodError when not present, even though the bytecode exists. |
| Fix | Two options: (a) **register a layout-neutral `String.charAt(I)C` in `register_essential_natives`** that uses `ctx.read_string` to be agnostic of JDK version (the recon hack uses this approach — see Session 94 worktree at `native-builtins/src/lib.rs:316`); (b) investigate why JMOD-loaded `java/lang/String` bytecode `charAt` is not being found by `find_method` and fix the dispatch. (a) is faster to ship; (b) is structurally correct and likely fixes a category of similar "essential natives only registered in synthetic mode" bugs. |
| Success | Reproducer no longer prints `NoSuchMethodError method="java/lang/String.charAt(I)C"`. Capture next failure verbatim. |
| Parallel-safe with | RKC16N.1, RKC16N.2, RKC16N.3, RKC16N.5. Single-file change (lib.rs) plus optionally one helper extraction in lang_string.rs. |
| Constraints | Don't touch `value_stack.rs` / Getfield-Putfield in `interpreter.rs` / `phases_late.rs::register_phase71_natives`. |
| Notes | Likely a category-class bug: `String.length()`, `String.substring(II)`, `String.indexOf(I)` may all be in the same boat. Agent should grep `register_synthetic_overrides` for `"java/lang/String"` registrations and audit which are also reachable in real-JDK mode. |

### RKC16N.9 — Investigate `org.jboss.modules.Module.<clinit>` NPE after RVERIF.2 lands — **RESOLVED in `8aad4c8`**

**Status (2026-04-29):** RESOLVED in commit `8aad4c8`. Actual root cause
was **not** a JBoss-Modules-specific helper as the original recon
guessed — it was a three-gap chain that left `java.lang.Void.TYPE` null
during JDK core bootstrap, which surfaced downstream as a
`Module.<clinit>` NPE. The fix landed three independent changes:
(a) jimage header version-decoding bug in `reader/src/jimage.rs` (read
as two `u16`s instead of one `u32` split into HIGH=major / LOW=minor —
inverted on little-endian disk); (b) missing `lib/modules` jimage
fallback in `vm/src/config.rs::discover_boot_classpath` (only checked
`rt.jar` and `jmods/`, missed JRE-style and jlink-trimmed runtimes
including the Adoptium "JDK 25" header dist used in the reproducer);
(c) `obj_arg` backtrace diagnostic added in `native-builtins/src/lib.rs`
gated on `CRATONVM_DBG_NULL_NATIVE` for future "which native got null"
investigations. The follow-up KC16 boot blocker is now **RKC16N.11**.

| Field | Value |
|---|---|
| ID | RKC16N.9 |
| Title | `Module.<clinit>` silent-swallow NPE leaves Module class half-initialised; main() then NPEs |
| Files | Investigation in `org.jboss.modules.Module` bytecode (extract from `/tmp/keycloak/keycloak-16.1.1/jboss-modules.jar`); fix likely in `native-builtins/src/jboss_module_loader.rs` (a missing-helper stub) or a new post-clinit fixup mirror to the existing `DefaultBootModuleLoaderHolder` one in `vm/src/vm/vm_util.rs`. |
| Reproducer (after RVERIF.2 + RKC16N.1/3/5/8 + Session 94 recon hacks) | `target/release/cratonvm.exe --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot" --Xmx 2g --jar /tmp/keycloak/keycloak-16.1.1/jboss-modules.jar -- -mp /tmp/keycloak/keycloak-16.1.1/modules org.jboss.as.standalone "-Djboss.home.dir=/tmp/keycloak/keycloak-16.1.1"` → `B6: silent-swallow class=org/jboss/modules/Module exc=java/lang/NullPointerException: null object argument` then `Exception in thread "main" java/lang/NullPointerException`. |
| Recon | (1) Run with `CRATONVM_STRICT_SWALLOWS=1` to escalate the first swallow to a panic with a real stack trace. (2) `javap -c -p` on `org/jboss/modules/Module` (extract from `jboss-modules.jar`); find every static field initialiser. (3) Identify which static-init expression returns null. Common candidates in JBoss Modules: `LOG_MANAGER` lookup, `MODULE_DIR` / `JBOSS_HOME_DIR` from system properties, `PathFilters.acceptAll()` static initialisation, or the synthetic `defaultClassFilter`. (4) Check `org/jboss/modules/Main.<clinit>` (which already runs cleanly per `-version`) for state it sets that `Module.<clinit>` then expects. |
| Fix direction | Two paths: (a) implement the missing helper(s) so the static init succeeds (e.g. register a native for `org/jboss/modules/PathFilters.acceptAll()` if it returns null today); (b) post-clinit fixup similar to `DefaultBootModuleLoaderHolder` (see `vm/src/vm/vm_util.rs`) — populate the field manually after the swallow. (a) is structurally correct; (b) is a session-bridging tactic. |
| Success | KC16 standalone progresses past `Module.<clinit>` and enters `org.jboss.modules.Main.run` proper. Next failure (if any) is downstream — module.xml parsing or class-resolution against deployed modules. |
| Parallel-safe with | Phase A–I items, RVERIF.2 (depends), RKC16N.1/3/5/8 (depend). |
| Constraints | No emojis. Don't touch `value_stack.rs` / Getfield-Putfield in `interpreter.rs` / `phases_late.rs::register_phase71_natives`. |

### RKC16N.11 — Diagnose opaque main()-thread NPE downstream of `ManagementFactory.<clinit>`

| Field | Value |
|---|---|
| ID | RKC16N.11 |
| Title | Diagnose opaque main()-thread NPE downstream of `ManagementFactory.<clinit>` |
| Files | TBD — depends on which Java frame the NPE escapes from. Recon-only iteration first; expected suspects (once the failing frame is identified) include `vm/src/vm/vm_init.rs` (further missing-native registrations), `native-builtins/src/jmx.rs` (RKC16N.10 added natives — one of them may be returning a value of the wrong type), `native-builtins/src/shared_secrets_bridge.rs` (legacy `BufferPool` bridge from RKC16N.10 — null is the right answer per spec, but a downstream caller may be deref'ing it without a null check), and any class whose `<clinit>` sits between `ManagementFactory` and `main()` in JBoss-Modules' boot chain. |
| Reproducer | `target/release/cratonvm.exe --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot" --Xmx 2g --jar /tmp/keycloak/keycloak-16.1.1/jboss-modules.jar -- -mp /tmp/keycloak/keycloak-16.1.1/modules org.jboss.as.standalone "-Djboss.home.dir=/tmp/keycloak/keycloak-16.1.1"` → `Exception in thread "main" java/lang/NullPointerException` (no message, no cause, no Java stack trace). Note: `ManagementFactory.<clinit>` records a silent-swallow `UnsatisfiedLinkError` immediately before this — that swallow is **unrelated** to the fatal NPE (it comes from one of `ManagementFactory`'s `Class.forName` try/catch probes, tolerated by design). |
| Recon | The immediate diagnostic gap is **no Java stack trace** in the CLI exception path. A parallel agent is working on adding stack-trace formatting at the top-level CLI exception print site (`vm-cli/src/main.rs`); this task is **gated on that work** and is pure recon for the first iteration. Once a Java stack trace is available: (1) capture stderr to `/tmp/kc16_post_rkc16n10.txt`; (2) read the throwing frame and the immediately-enclosing frames; (3) `javap -c -p` on the failing class (extract from JDK `lib/modules` if it's a JDK class, or from `jboss-modules.jar` if JBoss); (4) walk the bytecode at the failing PC to identify which value is null and trace it back to the producer (static field, constructor, factory method, or native return). Useful env knobs already in place: `CRATONVM_STRICT_SWALLOWS=1` (escalates first swallow to panic), `CRATONVM_DBG_NULL_NATIVE=1` (RKC16N.9's new `obj_arg` backtrace — emits a Rust backtrace when a native receives `Value::Object(None)`). |
| Fix direction | Once the failing frame is identified, the fix follows the historical RKC16N.9 pattern (which turned out to be a JDK-side gap, not a JBoss-side gap — keep an open mind about which side the missing piece is on). Two likely shapes: (a) add the missing native or static-init helper that produced the null — register in the appropriate real-JDK / synthetic registration path; or (b) walk back to the silent-swallow that left a static field null — either the `ManagementFactory` swallow noted above (if it turns out to be load-bearing despite looking by-design) or a different upstream swallow surfaced via `CRATONVM_STRICT_SWALLOWS=1`. Path (a) is the structurally-correct fix; (b) may need a post-clinit fixup mirror similar to `DefaultBootModuleLoaderHolder` if the real native isn't yet implementable. |
| Success | KC16 standalone progresses past the empty-message NPE; either `org.jboss.modules.Main.run` enters its main loop and starts opening `module.xml`, or the next failure is a different, named exception with a real stack trace (capture verbatim and file as RKC16N.12). |
| Parallel-safe with | Existing RKC16N.* items, but **serialised after the CLI stack-trace work lands** (parallel agent on `vm-cli/src/main.rs`). Also serialised after the parallel work on `native-builtins/src/jmx.rs` and `native-builtins/src/shared_secrets_bridge.rs` if those land first — they touch the most likely suspect surface. |
| Constraints | **Pure recon for the first iteration — no code changes until the failing frame is identified.** No emojis. Don't touch `value_stack.rs` / Getfield-Putfield in `interpreter.rs` / `phases_late.rs::register_phase71_natives`. |

### RKC16N.4 — Capture next KC16 blocker after RKC16N.1 lands

| Field | Value |
|---|---|
| ID | RKC16N.4 |
| Title | Re-run KC16 -version after RKC16N.1 ships and document the next NoSuchMethodError / livelock / NPE |
| Files | `docs/kc16-blocker-map.md` (append "Live status (Session 95+)"). |
| Reproducer | Same as RKC16N.1 reproducer, after that change is merged. Capture stderr to `/tmp/kc16_post_rkc16n1.txt`. |
| Success | docs/kc16-blocker-map.md updated with new "Live status" section showing the next failure mode (expected: CHM CAS livelock per RKC16N.2 — but verify, do not assume). |
| Blocks | Sequencing of any KC16N.5+ items. |
| Parallel-safe with | All non-doc work; serialised after RKC16N.1. |
| Constraints | Pure recon — no code changes. |
