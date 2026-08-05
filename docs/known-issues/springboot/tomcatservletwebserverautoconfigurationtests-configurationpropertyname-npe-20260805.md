# `TomcatServletWebServerAutoConfigurationTests` — NPE building a `BindException` message masks the real `ServerProperties` bind failure

**Status: OPEN — found 2026-08-05**

## Symptom

**8/18 tests fail.** All observed failures share the same shape: binding
`ServerProperties` (prefix `server`) throws
`ConfigurationPropertiesBindException`, but the exception's own
`toString()`/message-building throws a *second*, masking
`NullPointerException`:

```
java.lang.NullPointerException: Cannot invoke "org.springframework.boot.context.properties.source.ConfigurationPropertyName$ElementType.isIndexed()" because the return value of "org.springframework.boot.context.properties.source.ConfigurationPropertyName$Elements.getType(int)" is null
  org.springframework.boot.context.properties.source.ConfigurationPropertyName.isIndexed(ConfigurationPropertyName.java:121)
  org.springframework.boot.context.properties.source.ConfigurationPropertyName.buildDefaultToString(ConfigurationPropertyName.java:580)
  org.springframework.boot.context.properties.source.ConfigurationPropertyName.buildToString(ConfigurationPropertyName.java:566)
  org.springframework.boot.context.properties.source.ConfigurationPropertyName.toString(ConfigurationPropertyName.java:558)
  org.springframework.boot.context.properties.bind.BindException.buildMessage(BindException.java:81)
  org.springframework.boot.context.properties.bind.BindException.<init>(BindException.java:43)
  org.springframework.boot.context.properties.bind.Binder.handleBindError(Binder.java:419)
  org.springframework.boot.context.properties.bind.Binder.bind(Binder.java:378)
  org.springframework.boot.context.properties.bind.JavaBeanBinder.bind(JavaBeanBinder.java:127)
```

`ConfigurationPropertyName.Elements` lazily computes an `ElementType` per
element index; `getType(int)` returning `null` for an in-range index means
some `ConfigurationPropertyName` reaching `Binder.handleBindError` has an
element whose type was never populated — i.e. the *name itself* is
malformed/unusual in a way this Spring code doesn't expect, most likely
because it was built from an environment variable or system property whose
key CratonVM exposes in an unusual shape (extra/empty segment, wrong
separator handling, etc.) compared to what real JDK/HotSpot exposes for
the same process environment. This NPE is a secondary failure masking the
real, still-unknown, `server`-prefix bind error underneath (the original
`BindException` cause is never observed — its own message-building throws
first).

## Cross-check

HotSpot baseline passes cleanly: `TomcatServletWebServerAutoConfigurationTests`
18/18 (`hotspot-baseline-latest.tsv` row 125). Not a CRLF-fixture issue —
this is a runtime property-binding failure, not a resource read. Genuinely
CratonVM-specific.

No existing doc found for `ConfigurationPropertyName$Elements`,
`Elements.getType`, or this NPE shape under `docs/known-issues/` or
`docs/internal/`.

## Root cause

Not identified this session — needs the *underlying* `ConfigurationPropertyName`
value that trips `Elements.getType`, which requires either patching
`buildDefaultToString` temporarily to catch-and-print instead of throwing,
or attaching a debugger/print at `Binder.handleBindError` to dump the
`ConfigurationPropertyName` object before `BindException` construction
reaches the broken `toString()`. Prime suspect given the `server` prefix
and this being a webmvc/servlet-bootstrap-wide investigation: an unusual
environment variable or system property CratonVM sets or exposes (e.g. via
`System.getenv()`/`System.getProperties()` natives) with a name shape
(double underscore, trailing separator, empty component) that produces a
`ConfigurationPropertyName` with more/different elements than Spring's
relaxed-binding code expects.

## Affected classes

- `module/spring-boot-tomcat` — `org.springframework.boot.tomcat.autoconfigure.servlet.TomcatServletWebServerAutoConfigurationTests`
