# `HazelcastAutoConfigurationClientTests`: client XML config fails schema validation on a `kubernetes` element the schema doesn't expect

**Status: OPEN — found 2026-08-05**

## Symptom

```
com.hazelcast.config.InvalidConfigurationException: cvc-complex-type.2.4.a: Invalid content was found
  starting with element '{"http://www.hazelcast.com/schema/config":kubernetes}'. One of
  '{... import, config-replacers, cluster-name, ... network, ...}' is expected.
Caused by: org.xml.sax.SAXParseException; lineNumber: 114; columnNumber: 41; ...
```

The test's client XML config declares a `<kubernetes>` element that the XSD
schema Hazelcast validates against does not recognize at that position — a
version mismatch between the config file's expected schema and the schema
actually used to validate it.

## Not confirmed as CratonVM's fault vs. classpath-resolution artifact

HotSpot passes this class cleanly on the same fixture
(`hsfull-after-20260804-s4`, `tests=12 failed=0`), which rules out the XML
fixture itself being malformed for the version of `hazelcast-client` this
test expects. Two candidate explanations, not distinguished in the time
available:

1. **Classpath resource resolution bug**: CratonVM's classloader picks up
   the wrong `hazelcast-config-*.xsd` resource (e.g. from a different jar,
   or an older cached version) via `getResourceAsStream`/`URLClassLoader`
   resource lookup, so schema validation runs against a mismatched schema
   version even though the correct one is present on the classpath.
2. **Maven-cache artifact mismatch**: per the standing note on
   `@ClassPathOverrides`/`ModifiedClassPathClassLoader`-based tests
   resolving against `~/.m2/repository` — if this test (or a sibling in the
   same module) uses classpath overrides and the local Maven cache has a
   stale/mismatched `hazelcast-client` jar, this would produce exactly this
   symptom without being a CratonVM bug at all. Not verified: did not check
   whether `HazelcastAutoConfigurationClientTests` (or its superclass) is
   `@ClassPathOverrides`-annotated, or check `~/.m2` cache freshness for
   `hazelcast-client` for this run.

## Where to look next

1. Check whether the test class uses classpath overrides — if so, verify
   the resolved `hazelcast-client`/`hazelcast` jar versions on the CratonVM
   run match HotSpot's, and check `~/.m2/repository` for staleness first,
   per the standing guidance on cold/corrupted Maven caches.
2. If not a classpath-override test, compare
   `getResource("hazelcast-config-*.xsd")` resolution order/content between
   CratonVM and HotSpot for the exact same classpath — a resource-shadowing
   bug (multiple same-named resources across jars, wrong one picked) would
   explain a schema mismatch with an otherwise-correct fixture.

## Affected classes

- `module/spring-boot-hazelcast` — `org.springframework.boot.hazelcast.autoconfigure.HazelcastAutoConfigurationClientTests`
