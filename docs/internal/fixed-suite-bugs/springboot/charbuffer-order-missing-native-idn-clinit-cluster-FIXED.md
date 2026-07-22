# `CharBuffer.order()` has no native registration — poisons `java.net.IDN`'s `<clinit>` for the rest of the process (FAIL cluster + 1 fatal CRASH)

**Status: FIXED 2026-07-14** (the `order()` root cause described below).
Registered `order()` natively on `java/nio/CharBuffer` (native platform
order, matching real `HeapCharBuffer.order()`) and on the four
`ByteBufferAsCharBuffer{B,L,RB,RL}` view classes (fixed endianness per the
class-name suffix, matching real per-view-class overrides) —
`native-builtins/src/phases_late.rs::register_p62_char_buffer`. Verified
with a standalone repro (`java.net.IDN.toASCII("example.com")` at process
start): the `AbstractMethodError: java/nio/CharBuffer.order()...` is gone,
confirmed by direct re-run against a fresh build.

**Residual found while verifying this fix**: with `order()` no longer
throwing, `IDN.<clinit>` now runs further and hits a **different, deeper,
pre-existing bug** — `java.lang.ArrayIndexOutOfBoundsException` inside
`CharBuffer.getArray(I[CII)` (`CharBuffer.java:972`, the private bulk-get
helper), specifically inside its `ScopedMemoryAccess.copyMemory` fast path
(`isAddressable()` is unconditional `true` for every real `CharBuffer` per
its own concrete bytecode — see decompile below — so this path is always
taken, not just for direct buffers). `java.net.IDN.toASCII` therefore still
does not fully succeed end-to-end on this build; the Spring Boot classes in
this cluster will still FAIL/CRASH, just with the new, different exception
instead of the old `AbstractMethodError`/`NoClassDefFoundError` chain. This
residual has **not** been root-caused or fixed — it needs its own
investigation into `ScopedMemoryAccess.copyMemory`'s native implementation
(likely misinterpreting the heap-relative `address`/`ARRAY_BASE_OFFSET`
math for a non-direct `CharBuffer`'s backing array) and is filed separately:
[`../../known-issues/springboot/charbuffer-getarray-scopedmemoryaccess-copymemory-aioobe.md`](../../known-issues/springboot/charbuffer-getarray-scopedmemoryaccess-copymemory-aioobe.md).

---

**Original OPEN report follows (kept for the `order()` root-cause record):**

## Symptom

Several Spring Boot test classes report `java.lang.NoClassDefFoundError:
java/net/IDN` from inside Spring's own `ModifiedClassPathClassLoader`
plumbing (Spring's test-support classloader that isolates a per-test-class
classpath):

```
    => java.lang.NoClassDefFoundError: java/net/IDN
       org.springframework.util.ConcurrentReferenceHashMap$6.execute(ConcurrentReferenceHashMap.java:387)
       org.springframework.util.ConcurrentReferenceHashMap$Segment.doTask(ConcurrentReferenceHashMap.java:681)
       org.springframework.util.ConcurrentReferenceHashMap.doTask(ConcurrentReferenceHashMap.java:565)
       org.springframework.util.ConcurrentReferenceHashMap.computeIfAbsent(ConcurrentReferenceHashMap.java:381)
       org.springframework.boot.testsupport.classpath.ModifiedClassPathClassLoader.get(ModifiedClassPathClassLoader.java:114)
       org.springframework.boot.testsupport.classpath.ModifiedClassPathExtension.interceptMethod(ModifiedClassPathExtension.java:93)
       org.springframework.boot.testsupport.classpath.ModifiedClassPathExtension.interceptTestMethod(ModifiedClassPathExtension.java:76)
```

`core/spring-boot`'s `Log4J2LoggingSystemTests` (61 methods) shows this
pattern on 60 of its 61 failures. The 61st (and *first-executed*, see below)
fails with a **different-looking** symptom in the same class/process:

```
    => java.lang.AbstractMethodError: method java/nio/CharBuffer.order()Ljava/nio/ByteOrder; has no Code attribute
       java.nio.CharBuffer.getArray(CharBuffer.java:966)
       java.nio.CharBuffer.get(CharBuffer.java:838)
       java.nio.CharBuffer.get(CharBuffer.java:865)
       jdk.internal.icu.impl.ICUBinary.getChars(ICUBinary.java:277)
       jdk.internal.icu.util.CodePointTrie.fromBinary(CodePointTrie.java:268)
       jdk.internal.icu.impl.UCharacterProperty.<init>(UCharacterProperty.java:597)
       jdk.internal.icu.impl.UCharacterProperty.<clinit>(UCharacterProperty.java:630)
       jdk.internal.icu.lang.UCharacter.getUnicodeVersion(UCharacter.java:419)
       jdk.internal.icu.text.StringPrep.<init>(StringPrep.java:228)
       java.net.IDN.<clinit>(IDN.java:253)
       org.apache.http.conn.util.PublicSuffixMatcher.getDomainRoot(PublicSuffixMatcher.java:144)
       org.apache.http.conn.ssl.DefaultHostnameVerifier.matchIdentity(DefaultHostnameVerifier.java:204)
       ...
       org.eclipse.aether.internal.impl.DefaultArtifactResolver.performDownloads(DefaultArtifactResolver.java:537)
       ...
       org.springframework.boot.testsupport.classpath.ModifiedClassPathClassLoader.resolveCoordinates(ModifiedClassPathClassLoader.java:258)
       org.springframework.boot.testsupport.classpath.ModifiedClassPathClassLoader.getAdditionalUrls(ModifiedClassPathClassLoader.java:237)
       org.springframework.boot.testsupport.classpath.ModifiedClassPathClassLoader.processUrls(ModifiedClassPathClassLoader.java:222)
       org.springframework.boot.testsupport.classpath.ModifiedClassPathClassLoader.compute(ModifiedClassPathClassLoader.java:142)
       org.springframework.boot.testsupport.classpath.ModifiedClassPathClassLoader.lambda$get$0(ModifiedClassPathClassLoader.java:114)
       org.springframework.util.ConcurrentReferenceHashMap$6.execute(ConcurrentReferenceHashMap.java:387)
       ...
       org.springframework.boot.testsupport.classpath.ModifiedClassPathExtension.interceptTestMethod(ModifiedClassPathExtension.java:76)
```

(full log: `apps/spring-boot-suite-runner/.suite/results/crashfail-20260714/shard2/logs/core_spring-boot.org.springframework.boot.logging.log4j2.Log4J2LoggingSystemTests.out.log`)

The identical two-symptom pair (one `AbstractMethodError` + N
`NoClassDefFoundError: java/net/IDN`) also appears, unmodified, in:

- `core/spring-boot` `SpringProfileArbiterTests` (shard2)
- `core/spring-boot` `NoSuchMethodFailureAnalyzerTests` (shard1, 1
  `AbstractMethodError` + 3 `NoClassDefFoundError`)
- `core/spring-boot-test` `DuplicateJsonObjectContextCustomizerFactoryTests`
  (shard3)
- `module/spring-boot-flyway` `Flyway110AutoConfigurationTests` (shard5)
- `module/spring-boot-gson` `Gson210AutoConfigurationTests` (shard5)

(`ConditionalOnCheckpointRestoreTests` and
`WebMvcTestHtmlUnitWebClientIntegrationTests`, also named as candidates for
this cluster, do not appear anywhere under this run's
`shard*/logs/` — they were not part of this suite run and could not be
checked here.)

A separate, **fatal** occurrence crashes the whole VM process rather than
failing a test — `core/spring-boot-test`'s
`UriBuilderFactoryWebClientTests` (`shard3`):

```
Mockito is currently self-attaching to enable the inline-mock-maker. ...
[cratonvm] main-vm run() returned Err: Error in thread "main" linkage error: no class def found: java/net/IDN
[cratonvm] main-vm run() Err (debug): Error in thread "main" linkage error: no class def found: java/net/IDN
```

(full log:
`apps/spring-boot-suite-runner/.suite/results/crashfail-20260714/shard3/logs/core_spring-boot-test.org.springframework.boot.test.web.htmlunit.UriBuilderFactoryWebClientTests.err.log`)

## Root cause

**This is one bug, not two.** The `AbstractMethodError` on
`CharBuffer.order()` *is* the root cause of the entire `NoClassDefFoundError:
java/net/IDN` cluster, including the fatal crash — it is not a distinct,
unrelated symptom.

The `NoSuchMethodFailureAnalyzerTests` log (which orders its 4 failing test
methods so the trigger is visible) shows the mechanism directly: the
`AbstractMethodError` fires from *inside* `ModifiedClassPathClassLoader`'s own
machinery, not from Spring Boot application code under test.
`ModifiedClassPathClassLoader.resolveCoordinates` (line 258) uses Eclipse
Aether to resolve/download a differently-versioned dependency jar for the
test's isolated classpath. That triggers a real HTTPS connection to a Maven
repository via Apache HttpClient, whose `SSLConnectionSocketFactory` calls
`DefaultHostnameVerifier.verify()` → `PublicSuffixMatcher.getDomainRoot()` →
`java.net.IDN`. This is the **first** reference to `IDN` in the process, so
its static initializer (`IDN.<clinit>`, `IDN.java:253`) runs for the first
time — which constructs a `StringPrep`, which loads ICU4X Unicode character
property data (`UCharacterProperty.<clinit>` →
`jdk.internal.icu.impl.ICUBinary.getChars`), which reads a `char[]` out of a
`CharBuffer` via `CharBuffer.get(int[])` → `CharBuffer.getArray()` →
`CharBuffer.order()`.

`CharBuffer.order()` is abstract in real OpenJDK — every concrete leaf
subclass (`HeapCharBuffer`, `StringCharBuffer`,
`ByteBufferAsCharBuffer{B,L,RB,RL}`) overrides it. CratonVM never registers a
native `order()` for `java/nio/CharBuffer` or **any** of its concrete
subclasses:

- `native-builtins/src/servlet.rs` registers `order()` directly on
  `java/nio/ByteBuffer` (line 4453), `IntBuffer` (5187), `LongBuffer` (5201),
  `ShortBuffer` (5215), `FloatBuffer` (5229), `DoubleBuffer` (5243) — i.e.
  every other typed buffer view has its own concrete `order()` native.
- `native-builtins/src/phases_late.rs`'s `register_p62_char_buffer` (from
  line 29789) registers `allocate`, `wrap`, `hasArray`, `isReadOnly`,
  `isDirect`, `toString(II)`, `toString()`, `subSequence` directly on
  `java/nio/CharBuffer` and its subclass list (`ByteBufferAsCharBufferB/L/
  RB/RL`, `HeapCharBuffer`, `HeapCharBufferR`, `StringCharBuffer`, lines
  29983-30056+) — but **no `order()` anywhere in this file, nor anywhere else
  in `native-builtins/src/`** (`grep -rn '"order"' native-builtins/src/*.rs`
  has zero `CharBuffer` hits).

This is the exact same omission pattern already documented in this file's
own comment for `isReadOnly()`/`isDirect()` (phases_late.rs:29875-29888):
*"every OTHER typed view buffer ... gets these registered directly on its
own class, but CharBuffer was missing from that list"* — that comment
describes a fix that landed for `isReadOnly`/`isDirect`/`toString`/
`subSequence`, but `order()` was never added to the same fix, and it hits the
identical failure shape: CratonVM's generic "resolved method has no Code
attribute" path (`vm/src/runtime/interpreter.rs:4443`,
`"method {class}.{method}{descriptor} has no Code attribute"`) fires because
dispatch resolves all the way up to the abstract declaration in `CharBuffer`
with no native override anywhere in the hierarchy.

The `CharBuffer`/ICU4X/`StringPrep` stack frames in the log carry **real,
correct OpenJDK source line numbers** (`CharBuffer.java:838/865/966`,
`ICUBinary.java:277`, `CodePointTrie.java:268`, `IDN.java:253`, etc.) — this
is genuine real-JDK bytecode executing correctly up to the missing native,
not a classpath-search/stub-fallback artifact. (A parallel investigation
considered whether this was a `ModifiedClassPathClassLoader`-specific
classloading gap for `java.net.IDN` itself — e.g. a bootstrap/global-loader
special case — but `vm/src/runtime/interpreter.rs`'s
`is_global_resolution_namespace` hard-codes `java/`, `javax/`, `jdk/`,
`sun/`, `com/sun/` to always resolve through the single flat global class
store regardless of which classloader triggered the reference, so there is
no classloader-identity-specific path here. `java.net.IDN` resolves and
loads fine as a class; only its *static initializer* fails, for reasons
unrelated to which classloader referenced it.)

Once `IDN.<clinit>` throws (wrapping the `AbstractMethodError`), the JVM
class-initialization contract requires the class to remain permanently in
the "erroneous" state for the rest of that process: every subsequent active
use of `IDN` throws `NoClassDefFoundError` without re-running `<clinit>`.
That is exactly the 60/61, 3/4, etc. splits observed — one test method
(whichever happens to run first and be the first-ever reference to `IDN` in
that JVM process) pays the original `AbstractMethodError`; every other test
method in the same class/process pays the derived `NoClassDefFoundError`.
`UriBuilderFactoryWebClientTests`'s fatal crash is the same failure occurring
so early (effectively during process/main setup, before any per-test-method
try/catch exists to downgrade it to a reported `FAIL`) that it surfaces as an
uncaught top-level `linkage error`, terminating the whole run.

## Repro

Any CratonVM program that is the *first* in its process to reach
`java.net.IDN.toASCII`/`toUnicode` (directly, or transitively via Apache
HttpClient hostname verification, as here) will hit
`AbstractMethodError: java/nio/CharBuffer.order()...` once, then
`NoClassDefFoundError: java/net/IDN` on every subsequent call in the same
process. A minimal standalone repro should be: call
`java.net.IDN.toASCII("example.com")` directly at process start under
CratonVM (no Spring Boot or Aether required) — that alone exercises
`StringPrep`'s ICU data load and should reproduce the `AbstractMethodError`.

Suite-level repro, matching this run's invocation pattern
(`apps/spring-boot-suite-runner/run-spring-boot-suite.md`):

```powershell
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <TSV row: core/spring-boot	org.springframework.boot.logging.log4j2.Log4J2LoggingSystemTests> `
  -Start 1 -Count 1 -Exe <cratonvm exe>
```

or, for the fatal crash:

```powershell
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <TSV row: core/spring-boot-test	org.springframework.boot.test.web.htmlunit.UriBuilderFactoryWebClientTests> `
  -Start 1 -Count 1 -Exe <cratonvm exe>
```

Both TSV rows are present in
`apps/spring-boot-suite-runner/.suite/all-tests.tsv`.

## Related

- `native-builtins/src/phases_late.rs:29875-29888` — the sibling comment
  documenting the identical omission already fixed for
  `isReadOnly()`/`isDirect()`/`toString()`/`subSequence()` on `CharBuffer`;
  `order()` is the one method from that same audit that was never added.
- `docs/internal/fixed-suite-bugs/springboot-httpclient-builder-dead-registration-abstractmethoderror-FIXED.md`
  — an earlier, structurally similar "missing native registration →
  `AbstractMethodError`" cluster in this same suite, fixed by completing the
  registrar's method coverage; the fix for this bug is very likely the same
  shape (register `order()` on `java/nio/CharBuffer` and its concrete
  subclasses, mirroring the existing `ByteBuffer`/`IntBuffer`/`LongBuffer`/
  `ShortBuffer`/`FloatBuffer`/`DoubleBuffer` registrations in
  `native-builtins/src/servlet.rs`).
- `vm/src/runtime/interpreter.rs:4443` — the generic "resolved method has no
  Code attribute" error path that surfaces any such native-registration gap
  as `AbstractMethodError`.
- Investigated and **ruled out**: a classloader-identity-specific resolution
  gap for `java.net.IDN` under `ModifiedClassPathClassLoader`
  (`vm/src/runtime/interpreter.rs`'s `is_global_resolution_namespace`
  resolves all `java/*` references through one flat global class store
  regardless of the referencing classloader).
