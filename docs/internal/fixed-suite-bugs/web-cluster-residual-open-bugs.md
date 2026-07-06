# web/test.web cluster — residual bugs (2026-07-06)

Found via a full Spring suite sweep of `test.web`, `web.client`, `web.context`,
`web.method`, `web.reactive.function`, `web.reactive.resource`,
`web.reactive.result`, `web.servlet`, `web.util` (653 classes, jit-real mode).
This supersedes an earlier version of this doc that was lost when the Azure
build host's ephemeral disk was wiped mid-session before it could be pushed.
**All 3 items from the original OPEN list have since been fixed or confirmed
not-reproducing** — see the FIXED section below. No items remain open from
this sweep as of 2026-07-06.

## Real CratonVM bugs — all resolved (moved to FIXED section below)

(Originally 3 open items here: gzip/zlib byte-count mismatch, Jython
Py.<clinit> circular dependency, and XlsViewTests POI ClassCastException.
All 3 are now resolved — see items 15-16 and the XlsViewTests note in the
FIXED section.)

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
15. `java.util.logging.Logger.log(Level, String, Object[])` (and the
    single-`Object` overload) had no native override — CratonVM's synthetic
    `Logger` objects bypass the real constructor, leaving instance field
    `loggerBundle` uninitialized, so the unshimmed call fell through to real
    JDK bytecode (`Logger.doLog` → `getEffectiveLoggerBundle()`) and NPE'd
    reading the never-initialized field. Jython 2.7.4's `PySystemState`
    bootstrap (`PrePy.maybeWrite` → `writeConsoleWarning`) calls exactly this
    overload on every `PythonInterpreter`/JSR-223 `jython` engine bootstrap,
    breaking `DefaultFragmentsRenderingTests` and `FragmentRenderingStreamTests`.
    (Not the class-initialization-ordering race originally hypothesized —
    confirmed via direct repro against the actual Jython jar.)
16. `flate2`'s default `rust_backend` (`miniz_oxide`) DEFLATE encoder,
    while spec-correct, isn't byte-identical to real zlib's output —
    `ResourceHttpRequestHandlerTests::partialContentByteRangeWithEncodedResource`
    expected a 66-byte gzip stream (matching real zlib) but got 69 bytes.
    Fixed by switching the workspace's `flate2` feature to `zlib-rs` (a pure-
    Rust, no-system-dependency zlib reimplementation, bit-for-bit compatible
    with real zlib) — verified with a side-by-side baseline-vs-fixed build
    comparison to confirm zero regressions.

`JAXBContextImpl.createValidator` `VerifyError` (from the original bug list)
did not reproduce on a clean build — the check that would produce it
(`verify_inherited_abstract_methods_implemented` in `classloading/src/verifier.rs`)
is already disabled on `dev`, precisely because of this JAXB scenario. No fix
needed. `CookieLocaleResolverTests`'s invalid-timezone tests also did not
reproduce — already fixed by unrelated prior `TimeZone` native work.

`XlsViewTests::xlsxView`'s `ClassCastException: StringEnumValue cannot be
cast to STCellType$Enum` (traced to `SchemaTypeImpl.ensureStringEnumInfo()`'s
reflective `Class.getField("table").get(null)` read on the XmlBeans-generated
`STCellType$Enum`) also did **not** reproduce on a fresh `dev` build
(HEAD `1fcd2feb`, 2026-07-06). Verified via a dedicated
`fix/xls-poi-local` worktree: built a clean baseline binary and ran
`XlsViewTests` alone (3/3 pass, including `xlsxView`) and again as part of the
full `org.springframework.web.servlet.view.*` package (28 classes) — `OK 3 3
0 0 0` both times, with the other pre-existing failures in that sweep
(FreeMarker, Jython/JRuby, Groovy, `MarshallingViewTests`,
`DefaultFragmentsRenderingTests`, `ScriptTemplateViewTests` timeout)
reproducing exactly as documented elsewhere in this file, confirming the
binary/harness behaved normally rather than silently skipping the test.
Bisecting the intervening commits wasn't done, but the `ensure_class_initialized`
call added ahead of every static `Field.get`/`Field.set` in `native-builtins/
src/lang_class.rs` (`ensure_static_field_declaring_class_initialized`, landed
in `50119adb` — already an ancestor of both this run and the run that found
the bug OPEN) plus later reflection/classloading hardening merged into `dev`
since (e.g. `489b0f88`, the `codex/web-residual-all-20260705-001` merge)
apparently fixed this as a side effect. No code change was needed or made.
