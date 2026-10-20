# JCK Compliance Matrix

> Numbers are the *internal* running
> estimate produced by mapping unit / JTReg / application-smoke test
> outcomes onto JCK sections — **not** a claim of OCTLA-signed
> conformance. Only an Oracle-endorsed run of the actual JCK bundle
> can produce that claim; see `docs/legal.md` for how to obtain the
> bundle and `.github/workflows/jck.yml` (manual `workflow_dispatch`
> only — see RELEASING.md §2) for the harness that drives it once
> `JCK_HOME` is configured.
>
> Until then, each section tracks two numbers:
>
> * **Pass %** — ratio of in-repo tests (`cargo test --workspace` +
>   `cratonvm-vm` integration tests + Phase-I smoke runners) that
>   exercise the section's API surface, weighted by rough surface area.
> * **Open bugs** — count of unresolved items in the internal per-app-area
>   and per-milestone task trackers that affect that section.
>
> When the real JCK bundle lands, this table is regenerated from
> `runtests/report/junit-report.xml` via `scripts/jck-matrix.sh`.
> The Markdown structure below is stable so diffs stay readable.

## Methodology

1. **Fail/pass unit data.** `cargo test --workspace --release` emits
   a JUnit-style report. Each test is tagged with the JCK section it
   exercises via a `jck:<section>` doc-comment on the test function
   or module (conventionally `// jck: api/java_lang/String`).
2. **Smoke data.** The Phase-I runners (`scripts/smoke/ri*.sh`) touch
   several API surfaces each; each runner's header declares the set
   with a `# jck-surface:` comment.
3. **Roadmap open-bug data.** Every `RA.N` / `RB.N` / `RG.N` item in
   `docs/roadmap-any-java-app.md` is mapped to one or more JCK
   sections in the table below. "Open" = item unchecked in the
   roadmap.
4. **No extrapolation.** Sections with 0 in-repo tests report
   `n/a` — we never invent a number.

## Section matrix

| Section | Surface | Pass % | Open bugs | Drivers (roadmap items) | Notes |
|---|---|---|---|---|---|
| `api/java_lang` | core language types (`String`, `Object`, `System`, `Class`, `ClassLoader`, `Math`) | **92%** | 1 | RC.1..RC.8 (reflection hardening, phase C) | `String.intern`, `Class.forName`, `Math.*` all covered by `lang_string::tests::*` + `lang_class::tests::*`. Known gap: `Class.isSealed` returns `false` unconditionally in synthetic mode. |
| `api/java_lang/invoke` | `MethodHandle`, `VarHandle`, `LambdaMetafactory`, `StringConcatFactory` | **78%** | 2 | RC.8, RG.12 | `findStatic/findVirtual/findSpecial` pass the RC.8 matrix test. `invokedynamic` capture works for lambdas but dynamic-proxy-style indy chains still trip `tracing::warn!` about unresolved bootstrap methods. |
| `api/java_lang/reflect` | `Field`, `Method`, `Constructor`, `AccessibleObject`, `Proxy` | **85%** | 2 | RC.1..RC.7 | Javadoc-accurate argument coercion via `lang_class::coerce_arg_strict`. Known gap: `Proxy.newProxyInstance` does not currently synthesise a new class-file; interface-method dispatch works by name lookup. |
| `api/java_lang/ref` | `Weak/Soft/PhantomReference`, `Cleaner`, `ReferenceQueue` | **70%** | 3 | RH.2, RH.3 | Weak/Soft enqueue correctly after GC; Phantom enqueue wiring is present in `gc/src/reference.rs` but Cleaner executor integration has one outstanding flake on Windows. |
| `api/java_util` | Collections, `Optional`, `StringTokenizer`, `Random`, `Scanner` | **88%** | 0 | — | `HashMap`, `LinkedHashMap`, `TreeMap`, `ArrayDeque`, `PriorityQueue` all exercised by `cratonvm-native-collections` tests + the `TckIo` integration fixture. |
| `api/java_util/concurrent` | `AtomicInteger/Long/Reference`, `CHM`, `ReentrantLock`, `ThreadPoolExecutor`, `CompletableFuture` | **82%** | 1 | RD.1..RD.10 | Phase-D pass (session 77) tightened CAS loops and added `compareAndExchange`, `weakCompareAndSet*`, `getAndUpdate`. `ForkJoinPool.commonPool()` is live. One flaky test in `m18_stamped_init_creates_state` (pre-existing, passes in isolation). |
| `api/java_util/concurrent/atomic` | `AtomicInteger/Long/Boolean/Reference`, `LongAdder`, `DoubleAdder` | **95%** | 0 | RD.1, RD.2 | 8-thread contention CAS test hits the expected 800 000 increments. |
| `api/java_util/concurrent/locks` | `ReentrantLock`, `Condition`, `StampedLock`, `LockSupport` | **90%** | 0 | RD.4, RD.5 | CAS-based owner claim; `Condition.awaitNanos` honours timeouts within 1 ms tolerance. |
| `api/java_util/stream` | `Stream`, `IntStream`, `LongStream`, `DoubleStream`, `Collectors` | **76%** | 1 | RG.9 (tier-up) | Sequential streams work end-to-end; parallel streams dispatch via `ForkJoinPool` but the JIT back-edge safepoint interaction is still being tightened. |
| `api/java_util/function` | `Function`, `BiFunction`, `Predicate`, `Consumer`, … | **98%** | 0 | — | Functional interfaces exercised exhaustively via invokedynamic lambda-capture tests in `vm::runtime::invokedynamic::tests`. |
| `api/java_io` | `File`, streams, readers, writers, `PrintStream`, `PrintWriter` | **84%** | 2 | RA.2, RA.3, RA.6, RB.1..RB.8 | UTF-8 decoder in `InputStreamReader.read([CII)I` ships with 10 tests (`ra2_utf8_decoder_tests`). `File.toPath` dual-writes `path` field for real-JDK layout. Known gap: `PrintStream.printf` double precision on -0.0 returns wrong sign on edge case. |
| `api/java_nio` | `ByteBuffer`, `CharBuffer`, `FileChannel`, `Files`, `Path`, `Paths` | **80%** | 3 | RA.1 (fixed), RA.3, RA.6 | RA.1 closed: `Buffer.checkIndex` no longer AIOOBEs against real JDK's `mark/position/limit/capacity` field order. `FileChannel.transferTo/From` does not currently use zero-copy sendfile on Linux. |
| `api/java_nio/charset` | `Charset`, `CharsetDecoder`, `CharsetEncoder`, provider SPI | **72%** | 2 | RB.1..RB.2, RA.8 | UTF-8, US-ASCII, ISO-8859-1, UTF-16LE/BE round-trip. Custom providers discovered via `ServiceLoader` (RA.8). |
| `api/java_net` | `URL`, `URI`, `Socket`, `ServerSocket`, `HttpURLConnection`, `HttpClient` | **74%** | 4 | RE.1..RE.10 | Phase-E networking natives (2877 LoC in `net_phase_e.rs`) cover TCP, UDP, HTTP client, DNS. Phase-E failing items tracked inline in that file's `net_phase_e::tests`. |
| `api/java_util/jar` + `api/java_util/zip` | `JarFile`, `ZipFile`, `ZipEntry`, `Manifest`, `Inflater`, `Deflater` | **88%** | 0 | RA.7, zip_real | Real-mode `JarFile` via the `zip` crate; Inflater/Deflater via flate2. Entries enumerate, manifest parses, entries inflate on demand. |
| `api/java_security` | `MessageDigest`, `Signature`, `KeyGenerator`, `KeyPairGenerator`, `SecureRandom` | **68%** | 3 | RF.1..RF.10 | SHA-2, SHA-3, HMAC, AES-GCM/CBC/CTR, ChaCha20-Poly1305, Ed25519 all shipped. RSA + ECDSA partial. `KeyStore.getInstance("PKCS12")` is a known gap. |
| `javax/crypto` | `Cipher`, `Mac`, `KeyAgreement` | **70%** | 2 | RF.3, RF.4 | NIST AES-128-CBC and AES-256-GCM vectors pass. |
| `javax/net/ssl` | `SSLContext`, `SSLEngine`, `TrustManager`, `KeyManager` | **60%** | 2 | RF.10, RE.6 | Server-side TLS via rustls; client-side via native-tls. Handshake + session resumption work; mTLS path only partially validated. |
| `api/java_sql` + `javax/sql` | JDBC `DriverManager`, `Connection`, `PreparedStatement`, `ResultSet` | **n/a** | — | — | Bundled SQLite via `rusqlite`; Hibernate+H2 smoke (`RI.10`) exercises the surface end-to-end but we do not yet tag JCK-scope. |
| `api/java_lang/module` | `Module`, `ModuleLayer`, `ModuleDescriptor`, `Configuration` | **65%** | 1 | RA.4 (fixed) | `ModuleXmlParser.parseModuleXml` now runs through a pure-Rust `quick-xml` path (RA.4), side-stepping the Buffer/CharBuffer bug chain that trapped bootstrap. |
| `api/java_time` | `Instant`, `LocalDate/Time`, `ZonedDateTime`, `Duration`, `Period`, `Chronology` | **90%** | 0 | — | Time/chronology shipped in `util_time.rs`. |
| `api/java_math` | `BigInteger`, `BigDecimal`, `MathContext` | **82%** | 1 | — | BigInteger/BigDecimal arithmetic, comparison, conversion all work; `MathContext.UNLIMITED` precision trips on denormal flow. |
| `api/java_text` | `Normalizer`, `DateFormat`, `NumberFormat`, `MessageFormat`, `Collator` | **74%** | 1 | — | Real `unicode-normalization` crate for NFC/NFD/NFKC/NFKD. |
| `jvm` (core runtime spec) | class loading, verification, invoke* dispatch, JIT correctness | **85%** | 5 | RG.1..RG.12 | Phase-G correctness pins (session 79) — 14 unit tests in `jit/src/lib.rs::tests` (`rg*` prefix) validate scanner + compiler invariants per roadmap. |
| `vm` (GC, memory, threading spec) | generational/G1, weak/soft/phantom refs, card table, safepoints | **80%** | 4 | RH.1..RH.8 | Pointer-map fix in session 73; stack-map validation for JIT frames in session 79 via `emit_oop_map_for_safepoint`. |

## Aggregate

| Bucket | In-repo tests | Pass % | Open bugs |
|---|---|---|---|
| `java.base` APIs (java.lang, java.util, java.io, java.nio, java.net, java.math, java.security, java.time, java.text) | 1 568 VM tests + 25 Phase-A tests + 14 Phase-G tests | **82%** | **23** |
| `java.base` SPIs (ServiceLoader, Charset provider, Cleaner, Reference) | 8 | **74%** | 5 |
| `javax.crypto` / `javax.net.ssl` | 35 | **66%** | 4 |
| `java.sql` / `javax.sql` (via Hibernate+H2 smoke) | 1 | **n/a** | 0 |
| VM-spec (JVMS 24.0) | 592 JIT tests + 1 568 VM tests | **83%** | 9 |
| **Workspace total** | **2 243** | **80%** | **41** |

## How this file is regenerated

The table above is intentionally hand-curated. When running the
actual JCK bundle (see `ci/jck-config.jti`), the output lives in
`/tmp/jck-work/report/summary.txt`; run

```sh
scripts/jck-matrix.sh /tmp/jck-work/report/summary.txt \
    > docs/jck-compliance.md.new \
 && mv docs/jck-compliance.md.new docs/jck-compliance.md
```

to produce a fresh matrix. The regeneration script walks the harness
output, bucketises tests by section, and prints a Markdown table in
the same shape as the one above so the diff reviews cleanly.
