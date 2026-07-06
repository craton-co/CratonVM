# web/test.web cluster — residual OPEN bugs (2026-07-06)

Found via a full Spring suite sweep of `test.web`, `web.client`, `web.context`,
`web.method`, `web.reactive.function`, `web.reactive.resource`,
`web.reactive.result`, `web.servlet`, `web.util` (653 classes, jit-real mode).
This supersedes an earlier version of this doc that was lost when the Azure
build host's ephemeral disk was wiped mid-session before it could be pushed;
most of what it listed has since been fixed (see the FIXED section below) —
only 3 items remain genuinely open.

## Real CratonVM bugs still OPEN

- **`ResourceHttpRequestHandlerTests::partialContentByteRangeWithEncodedResource(GzippedFiles)`**
  — `expected "bytes 0-1/66", was "bytes 0-1/69"`. CratonVM's `flate2`-backed
  `Deflater` (`native-builtins/src/zip_real.rs`) produces 69 bytes for the
  test fixture at every compression level, while real gzip/zlib produces
  exactly 66 — `flate2`'s default `miniz_oxide` backend, while spec-correct,
  isn't byte-identical to zlib's output. A real fix (switching to the
  `flate2/zlib` feature with vendored zlib) has workspace-wide blast radius
  and cross-platform (Windows) build risk — needs validation on both Linux
  and Windows before landing.
- **`DefaultFragmentsRenderingTests::render()`** (`ExceptionInInitializerError`)
  and **`FragmentRenderingStreamTests`** (`streamWithFlux`/`streamWithSseEmitter`,
  `IllegalStateException: Failed to send [...ResponseBodyEmitter$DataWithMediaType...]`)
  — same root cause: `Caused by: ExceptionInInitializerError` at
  `PyType.fromClass` → `PyJavaType.<clinit>` → NPE on `Py.threadStateMapping`.
  Confirmed CratonVM-specific (identical jar+JDK25 works under real HotSpot).
  Traced to a circular class-init dependency inside Jython 2.7.4's
  `Py.<clinit>`: constructing `PyNotImplemented`/`PyEllipsis` triggers
  `PyType.fromClass` → `PyJavaType` before `Py`'s own clinit reaches the
  `threadStateMapping` assignment. Needs the same kind of "pre-initialize
  before claim" workaround already used for the `PrimitiveClassDescImpl`/
  `ConstantDescs` cycle — the precise trigger point wasn't pinned down
  confidently enough yet to write a safe patch.
- **`XlsViewTests::xlsxView`** — `ClassCastException: StringEnumValue cannot
  be cast to STCellType$Enum`. Traced to `SchemaTypeImpl.ensureStringEnumInfo()`
  reflectively reading a static `table` field via `Class.getField("table").get(null)`;
  this fails under CratonVM in the full POI/XSSFWorkbook context but succeeds
  in an isolated repro using identical jars/classpath — the divergence is
  context-dependent on the exact object graph POI builds, needs deeper tracing
  than a first pass gave it.

## Not CratonVM bugs — environment/test-classpath gaps (skip)

- **FreeMarker** (`org/springframework/*/view/freemarker/FreeMarkerConfigurer`
  `NoClassDefFoundError`) — FreeMarker isn't on this suite run's test
  classpath. Environment/build-config gap, not a VM bug.
- **`org/springframework/oxm/jaxb/Jaxb2Marshaller`** `NoClassDefFoundError` —
  spring-oxm module not on classpath for these tests. Same category.
- **`sun/nio/ch/{IOUtil.fdLimit, EPollSelectorImpl, NativeThread}`** — the
  long-documented Linux-only native gap (see
  `azure-host-jdk21-linux-only-native-gaps` memory — blocks HttpClient5/Netty
  "Reactor Netty" test variants specifically on the Linux probe host; not a
  real bug, don't chase).
- **`os error 11` ("Resource temporarily unavailable") on RestClient/WebClient
  localhost connections** — observed under heavy host contention (many
  concurrent sessions sharing the Azure probe box); did not reproduce as a
  consistent CratonVM defect distinct from host load.
- **`Cp1047` charset `SkipException`** (`DefaultResponseCreatorTests`) — the
  test itself skips when the JVM doesn't support this charset; not a failure.
- **`UriComponentsBuilderTests::fromOpaqueUri()`** — a `java.net.URI`
  opaque-fragment test that doesn't use `WhatWgUrlParser` at all; pre-existing,
  unrelated to any fix in this cluster.
- **A Windows-specific `BeanDefinitionStoreException: I/O failure during
  classpath scanning`** observed on `GlobalCorsConfigIntegrationTests` when
  verifying on a Windows host — not reproduced as related to any fix here
  (none of the 12 fixes below touch classpath scanning); looks like a
  Windows path/file-locking artifact. Worth a second look if it recurs.

## FIXED this cluster (for cross-reference — see commit messages on `dev`)

1. `Collections.emptyListIterator()` mis-stamped as `EmptyIterator` (no
   `hasPrevious`) instead of `EmptyListIterator` — crashed Jetty main-thread.
2. `StringBuilder`/`StringBuffer`/`AbstractStringBuilder` missing
   `codePointAt`/`codePointBefore`/`codePointCount`/`appendCodePoint`
   natives — corrupted `WhatWgUrlParser`'s state machine (non-deterministic
   AIOOBE/AssertionError).
3. `register_p68_security_cert` (Certificate/X509Certificate natives) was
   gated behind the `synthetic-jdk` feature and never called in real-JDK
   builds — `Certificate.getEncoded()` → `AbstractMethodError` on any cert
   access (e.g. OkHttpClient's `TrustManagerFactory` init).
4. `System.setProperty`/`clearProperty` didn't update the `Properties`
   side-table backing an already-cached `System.getProperties()` singleton —
   broke SpEL `#{systemProperties.x}` for properties set post-`refresh()`.
5. TYPE_USE annotations on generic type args (`@Valid` on `List<@Valid X>`)
   parsed but discarded before reaching `AnnotatedParameterizedType
   .getAnnotatedActualTypeArguments()` — broke container-element `@Valid`
   cascading validation.
6. `Class.getPackage()` allocated a fresh `Package` per call instead of
   interning by name — broke `Package.equals()`.
7. Reflective `Method.invoke(null-arg, primitive-param)` threw a cause-less
   `IllegalArgumentException` instead of chaining a `NullPointerException`
   cause.
8. `java.net.URI.equals()`/`hashCode()` compared by exact raw-string equality
   instead of RFC 3986 §3.2.2 case-insensitive scheme/host comparison — also
   fixed the WHAT_WG IPv6-host-casing residual from an earlier pass of this
   doc (`UriComponentsTests::toUriWithIpv6HostAlreadyEncoded`), since that was
   actually a `URI.equals()` bug, not a `WhatWgUrlParser` bug.
9. `file:` URL connection handling didn't percent-decode the path before
   filesystem access — broke resource lookups with encoded characters
   (`ResourceHttpRequestHandlerIntegrationTests`, `%20` in filenames).
10. `Arrays$ArrayList`'s backing-array slot index differs between synthetic
    and real-JDK mode (real mode has `AbstractList.modCount` pushing it to
    slot 1) — `collect_collection_elements()` silently treated non-empty
    `Arrays.asList(...)`-backed collections as empty (broke
    `MockHttpServletRequest.setPreferredLocales`, hence
    `AcceptHeaderLocaleResolverTests`).
11. `Collections.unmodifiableSet()`'s synthetic stamp unconditionally
    declared `SortedSet`/`NavigableSet` — mis-triggered `AssertJ`'s
    `comparator()` probe on a plain `LinkedHashSet` wrapper
    (`RequestMappingInfoHandlerMappingTests`).
12. `synthetic_implements`'s name-based `instanceof Collection` fallback
    matched on the substring "Collection" inside "Collections$SingletonMap"
    itself — broke Groovy's `DefaultTypeTransformation.asCollection`
    (`ViewResolutionIntegrationTests::groovyMarkup`).
13. `kotlin.collections.ArrayAsCollection`'s `(values, isVarargs)` layout was
    misread by the generic "ArrayList-shaped" heuristic (boolean `isVarargs`
    treated as `size`) — truncated any `ArrayList(mutableListOf(...))` to one
    element (`InvocableHandlerMethodKotlinTests`).
14. `IntStream`/`LongStream`/`DoubleStream.iterator()` had no native
    registration — dispatched to the abstract `BaseStream.iterator()`
    interface declaration → `AbstractMethodError` (`XlsViewTests::xlsxStreamingView`).

`JAXBContextImpl.createValidator` `VerifyError` (from the original bug list)
did not reproduce on a clean build — the check that would produce it
(`verify_inherited_abstract_methods_implemented` in `classloading/src/verifier.rs`)
is already disabled on `dev`, precisely because of this JAXB scenario. No fix
needed. `CookieLocaleResolverTests`'s invalid-timezone tests also did not
reproduce — already fixed by unrelated prior `TimeZone` native work.
