# Spring CacheAdviceNamespaceTests XML schema parsing timeout - FIXED

`org.springframework.cache.config.CacheAdviceNamespaceTests` used to time out on CratonVM real-JDK mode because each inherited JUnit method rebuilt a `GenericXmlApplicationContext` from `cache-advice.xml`, causing repeated real-JDK Xerces schema loading/validation.

Fix summary:

- Added a native bridge for `DefaultDocumentLoader.createDocumentBuilderFactory(int, boolean)` that preserves Spring's validation and namespace-aware settings.
- Attached a VM-wide Xerces `XMLGrammarPoolImpl` to XSD-validating document builder factories so repeated Spring namespace loads reuse parsed grammars.
- Forced the bridge to win over the protected concrete Spring method at VM dispatch time.

Verification on 2026-07-09 with the unique binary `target/release/java-spring-cache-spel-20260709-001`:

- `./gradlew --no-daemon -I /tmp/cratonvm-spring-target-test-exec-20260709-001.gradle :spring-context:test --tests org.springframework.cache.config.CacheAdviceNamespaceTests --rerun-tasks --stacktrace`
- Result: `BUILD SUCCESSFUL`; test report recorded 65 tests, 0 failures, 0 errors, time `1.968` seconds under CratonVM.
