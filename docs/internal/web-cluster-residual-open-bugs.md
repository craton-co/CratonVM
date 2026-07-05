# web/test.web cluster — residual OPEN bugs (2026-07-04)

Found via a full Spring suite sweep of `test.web`, `web.client`, `web.context`,
`web.method`, `web.reactive.function`, `web.reactive.resource`,
`web.reactive.result`, `web.servlet`, `web.util` (653 classes, jit-real mode)
against `dev` @ 36d3102bf (which already includes the 7 fixes landed in this
same sweep — see the "web cluster" memory entry / commit messages on
`fix/jetty-iter-azure`, `fix/whatwg-azure`, `fix/cert-code-azure`,
`fix/method-azure` for the FIXED half of this cluster).

None of the items below were fixed in this pass — either lower priority than
the confirmed fixes, needs more investigation time, or (for the dependency
gaps) out of CratonVM's scope entirely. Filed here so a future session doesn't
have to re-discover them from a fresh suite run.

## Real CratonVM bugs (candidates for a future fix session)

- **`UriComponentsTests::toUriWithIpv6HostAlreadyEncoded[2]` (WHAT_WG)** —
  deterministic (not flaky): IPv6 host hex digits get lowercased even when
  `UriComponentsBuilder...build(true)` ("already encoded") should pass the
  host through verbatim. See `WhatWgUrlParser.java` IPv6 serialization. Found
  right after the (separately fixed) non-deterministic AIOOBE in the same
  parser — check whether HotSpot itself passes this test on the current
  spring-framework source before assuming it's CratonVM's fault.
- **`java.util.LinkedHashSet.comparator()` → `NoSuchMethodError`** — hit by
  `RequestMappingInfoHandlerMappingTests::getHandlerRequestMethodNotAllowed`.
  Looks like the same native-collections "empty/synthetic collection
  mis-stamped" bug family as the (fixed) `Collections$EmptyIterator`/
  `Collections$SingletonMap` cases — LinkedHashSet dispatch is picking up a
  method it shouldn't have / lacks one it needs.
- **`java.util.Collections$SingletonMap.iterator()` → `NoSuchMethodError`** —
  hit by `ViewResolutionIntegrationTests::groovyMarkup` (GroovyMarkupConfigurer
  bean creation). Same family as above; the fixed `1ae3066c5` covered
  `emptyListIterator`/`newSetFromMap` stamps but not this one.
- **`JAXBContextImpl.createValidator` → `VerifyError: concrete class must
  implement abstract method`** — hits ~10 tests across `test.web` and
  `web.servlet` (all the Xpath/XmlContent matcher/assertion tests). Possibly
  the SAME family as the (fixed) `Certificate.getEncoded()`/`X509Certificate`
  native-registration gap (register_p68_security_cert-style — check whether
  JAXB's classes have an analogous real-JDK-mode registration gap), or a
  genuine classfile-verifier bug misjudging abstract-method coverage.
- **`java.util.stream.IntStream.iterator()` → `AbstractMethodError: has no
  Code attribute`** — hit by `XlsViewTests::xlsxStreamingView`. Same "no Code
  attribute" bug family as the two above — three-plus instances now, strong
  candidate for a shared root cause worth hunting generically rather than
  patching one class at a time.
- **`AcceptHeaderLocaleResolverTests`** (5/5 methods) — `NoSuchElementException:
  List is empty`. Accept-Language header locale resolution;  not yet
  triaged this session.
- **`CookieLocaleResolverTests::resolveLocaleContextWithInvalidTimeZone{,OnErrorDispatch}`**
  — plain `AssertionError`, not yet triaged.
- **`InvocableHandlerMethodKotlinTests`** (`web.reactive.result`) — Kotlin
  default-parameter-value handling: `defaultValues()`/`defaultValuesOverridden()`
  throw `IllegalStateException: argument type mismatch` instead of using the
  default; `suspendingDefaultValueOverridden()` returns the default instead of
  the overridden value. Smells like a real Kotlin-interop/reflection bug in
  how CratonVM resolves default-value masks for suspend/coroutine methods.
- **`web.servlet.resource.ResourceHttpRequestHandlerIntegrationTests`** (4
  parameterized cases) and **`ResourceHttpRequestHandlerTests::partialContentByteRangeWithEncodedResource(GzippedFiles)`**
  — plain `AssertionError`s around path-pattern/gzip-encoded resource serving,
  not yet triaged.
- **`web.servlet.view.DefaultFragmentsRenderingTests::render`** —
  `ExceptionInInitializerError: null`, not yet triaged.
- **`web.servlet.mvc.method.annotation.FragmentRenderingStreamTests`** (both
  methods) — `IllegalStateException: Failed to send [...ResponseBodyEmitter
  DataWithMediaType...]`, streaming/emitter write failure, not yet triaged.
- **`web.servlet.view.document.XlsViewTests::xlsxView`** — Apache POI
  `PartAlreadyExistsException` on `/xl/styles.xml` — could be a real bug in
  how CratonVM's zip/stream natives interact with POI's OOXML package writer
  (duplicate part write), not yet triaged.

## Not CratonVM bugs — environment/test-classpath gaps (skip)

- **FreeMarker** (`org/springframework/*/view/freemarker/FreeMarkerConfigurer`
  `NoClassDefFoundError`, ~15 occurrences across `web.servlet.config`/
  `web.servlet.view`/`web.reactive.result`) — FreeMarker isn't on this suite
  run's test classpath. Environment/build-config gap, not a VM bug.
- **`org/springframework/oxm/jaxb/Jaxb2Marshaller`** `NoClassDefFoundError`
  (`test.web.servlet.samples.*.ViewResolutionTests`) — spring-oxm module not
  on classpath for these tests. Same category.
- **`sun/nio/ch/{IOUtil.fdLimit, EPollSelectorImpl, NativeThread}`** — the
  long-documented Linux-only native gap (see
  `azure-host-jdk21-linux-only-native-gaps` — blocks HttpClient5/Netty
  "Reactor Netty" test variants specifically on this Linux probe host; not a
  real bug, don't chase). Affects a large fraction of the `[3] Reactor Netty`
  / some `[2] Jetty Core` / `[4] Tomcat` parameterizations across
  `web.reactive.*` — expect these to keep failing here regardless of any
  CratonVM fix, and re-check on Windows/a real JDK≥19 host if it matters.
- **`os error 11` ("Resource temporarily unavailable") on RestClient/WebClient
  localhost connections** — observed under heavy host contention (60+
  concurrent sessions sharing this Azure probe box); did not reproduce this
  as a consistent CratonVM defect distinct from host load. Re-test on a quiet
  box before concluding it's a real client-retry bug.
- **`Cp1047` charset `SkipException`** (`DefaultResponseCreatorTests`) — the
  test itself skips when the JVM doesn't support this charset; not a failure.

## Fixed this session (for cross-reference — see commit messages on `dev` for full detail)

1. `Collections.emptyListIterator()` mis-stamped as `EmptyIterator` (no
   `hasPrevious`) instead of `EmptyListIterator` — crashed Jetty main-thread
   with `NoSuchMethodError`. (Reapplied `1ae3066c5`'s fix; this specific
   worktree had forked before that commit landed on `dev`.)
2. `StringBuilder`/`StringBuffer`/`AbstractStringBuilder` had no native
   registration for `codePointAt`/`codePointBefore`/`codePointCount`/
   `appendCodePoint` — fell through to real-JDK bytecode assuming a
   compact-string layout CratonVM doesn't use, corrupting
   `WhatWgUrlParser`'s state machine (non-deterministic AIOOBE/AssertionError).
3. `register_p68_security_cert` (Certificate/X509Certificate natives) was
   only reachable behind the `synthetic-jdk` feature gate and never called in
   real-JDK-mode builds — `Certificate.getEncoded()` had no natives and no
   bytecode, `AbstractMethodError` on any cert access (e.g. OkHttpClient's
   `TrustManagerFactory` init, hit regardless of whether the test itself uses
   HTTPS).
4. `System.setProperty`/`clearProperty` didn't update the `Properties`
   side-table backing an already-cached `System.getProperties()` singleton —
   broke SpEL `#{systemProperties.x}` resolution for properties set after
   `ApplicationContext.refresh()`.
5. TYPE_USE annotations on generic type arguments (e.g. `@Valid` on
   `List<@Valid Person>`) were parsed but discarded before reaching
   `AnnotatedParameterizedType.getAnnotatedActualTypeArguments()` — broke
   Spring's container-element `@Valid` cascading validation detection.
6. `Class.getPackage()` allocated a fresh `Package` object per call instead
   of interning by name — broke `Package.equals()` (which, like HotSpot, has
   no override and relies on identity/interning) wherever Spring compares
   packages by equality.
7. Reflective `Method.invoke` with a null argument against a primitive
   parameter threw a cause-less `IllegalArgumentException`; HotSpot chains a
   `NullPointerException` cause that Spring's error-message logic depends on.
