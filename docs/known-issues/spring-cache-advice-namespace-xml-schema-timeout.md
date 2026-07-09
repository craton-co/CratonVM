# Spring CacheAdviceNamespaceTests XML schema parsing timeout

`org.springframework.cache.config.CacheAdviceNamespaceTests` remains slow/timeouts on CratonVM real-JDK mode on Linux.

Observed on branch `codex/misc-small-linux-20260708` after the `CompletableFuture.completeValue(null)` fix:

- HotSpot baseline: `OK`, 65/65 tests, about 6 seconds.
- CratonVM: cache service method probes pass, including `classPutEvaluatesUnlessBeforeKey` and the null `@CachePut(unless = "#result == null")` path.
- Full class still times out because each JUnit method recreates `GenericXmlApplicationContext` from `cache-advice.xml`; Craton spends most time in real JDK Xerces XML schema validation/loading.

Representative watchdog stack:

- `CacheAdviceNamespaceTests.getApplicationContext`
- `GenericXmlApplicationContext.<init>` / `load`
- `XmlBeanDefinitionReader.doLoadDocument`
- `DocumentBuilderImpl.parse`
- `XMLSchemaValidator.findSchemaGrammar`
- `XMLSchemaLoader.loadSchema`
- `XSDHandler.parseSchema`
- `SchemaDOMParser.parse`

This appears to be a broader XML/Xerces schema-loading performance issue rather than a cache advice semantic bug. A follow-up should investigate caching or replacing the repeated schema-validation path without breaking Spring XML namespace handling.
