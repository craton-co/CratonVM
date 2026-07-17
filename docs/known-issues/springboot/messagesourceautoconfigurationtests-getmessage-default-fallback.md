# `MessageSourceAutoConfigurationTests` — `getMessage()` repeatedly falls through to the caller's default value instead of resolving the configured `.properties` message bundle

**Status: OPEN — found 2026-07-17**

## Symptom

6 of 19 tests fail. All 5 `AssertionFailedError`-shaped failures show the
literal caller-supplied default value coming back instead of the value the
target `.properties` bundle should have supplied — every test calls
`context.getMessage("foo", null, "Foo message", Locale.UK)` (the JDK/Spring
`MessageSource.getMessage(code, args, defaultMessage, locale)` 4-arg
overload — `"Foo message"` is literally the default-if-not-found argument,
not a message-bundle value), and every failing test gets back exactly that
default instead of the bundle-specific value it configured:

```
=> org.opentest4j.AssertionFailedError:
expected: "bar"
 but was: "Foo message"
       org.springframework.boot.autoconfigure.context.MessageSourceAutoConfigurationTests.lambda$propertiesBundleWithDotIsDetected$0(MessageSourceAutoConfigurationTests.java:82)
```

```
=> org.opentest4j.AssertionFailedError:
expected: "Some text with some swedish öäå!"
 but was: "Foo message"
       org.springframework.boot.autoconfigure.context.MessageSourceAutoConfigurationTests.lambda$testEncodingWorks$0(MessageSourceAutoConfigurationTests.java:91)
```

A 6th failure, without a caller-supplied default, surfaces the same
underlying miss as a real exception instead of a silently-substituted
default:

```
=> org.springframework.context.NoSuchMessageException: No message found under code 'foo' for locale 'en_US'.
       org.springframework.context.support.AbstractMessageSource.getMessage(AbstractMessageSource.java:162)
       org.springframework.boot.autoconfigure.context.MessageSourceAutoConfigurationTests.lambda$messageSourceWithNonStandardBeanNameIsIgnored$0(MessageSourceAutoConfigurationTests.java:231)
```

`tests=19 failed=6 skipped=1`. Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.context.MessageSourceAut-b92b4bc9771b.out.log`

## Root cause (hypothesis, not confirmed against CratonVM source)

Every failing test exercises a different sub-scenario of Spring Boot's
auto-configured `ResourceBundleMessageSource` (dot-separated basename
detection, slash-separated basename detection, non-UTF-8 encoding, multiple
`MessageSource` beans, an ignored non-standard bean name), each pointed at a
different classpath-relative `.properties` fixture — yet every one comes
back with the *identical* wrong value or the *identical* "not found"
exception, never a differently-wrong value. That consistency is the key
signal: a genuine per-fixture parsing bug (e.g. a bad `.properties` escape
decoder, or wrong-charset decoding) would be expected to produce varied,
scenario-specific wrong content; getting back the exact literal
default-argument value (or a hard miss) for every distinct basename/locale
combination instead points at the underlying
`ResourceBundleMessageSource`/`java.util.ResourceBundle` lookup never
actually locating *any* of these test-fixture bundles on the classpath, for
every basename tried, and Spring's `AbstractMessageSource` correctly falling
back to the caller's default (or throwing `NoSuchMessageException` when
there is no default) exactly as designed when a bundle genuinely can't be
found.

This module's `docs/internal/CRATONVM_BUGS/BUG-L-resourcebundle-locale-resolution.md`
already documents (and marks FIXED) three separate CratonVM-specific
`ResourceBundle.getBundle`/`Locale` native gaps — including "`getBundle`
ignored the locale and never threw CNFE" — in the same general area
(resource-bundle/locale natives), but for a `Tomcat`-suite target
(`StringManager`), not this Spring MessageSource path, and none of that
fix's three sub-fixes obviously explain "always the literal default
argument for every basename" the way a basename/classloader resolution
failure specific to *this* call shape would. Not confirmed as the same
gap — flagged as the most relevant prior art, not a proven match. No
standalone `ResourceBundleMessageSource`/`ResourceBundle.getBundle` probe
against these exact fixture files
(`apps/spring-boot/core/spring-boot-autoconfigure/src/test/resources/org/springframework/boot/autoconfigure/context/`)
was run this session to pin the exact failing lookup.

## Affected classes

| Module | Class |
|---|---|
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.context.MessageSourceAutoConfigurationTests` |
