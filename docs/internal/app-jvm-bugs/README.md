# App JVM bugs — detailed write-ups

One markdown file per CratonVM bug found while running real application test suites
(H2, WildFly, Hibernate, Elasticsearch, etc.). Each app’s `CRATONVM_BUGS.md` is the
short index; these files are the full handoff notes for fix agents.

| ID | File | App | Status |
|----|------|-----|--------|
| H2-1 | [bug-h2-securerandom-sha1prng.md](bug-h2-securerandom-sha1prng.md) | H2 | OPEN |
| H2-2 | [bug-h2-prepared-statement-column-count.md](bug-h2-prepared-statement-column-count.md) | H2 | OPEN |
| H2-3 | [bug-h2-mvstore-sys-lock-timeout.md](bug-h2-mvstore-sys-lock-timeout.md) | H2 | OPEN |
| H2-4 | [bug-h2-arraylist-toarray-multidim.md](bug-h2-arraylist-toarray-multidim.md) | H2 | OPEN (latent) |
| H2-5 | [bug-h2-operand-stack-underflow.md](bug-h2-operand-stack-underflow.md) | H2 | OPEN (latent) |
| WF-1 | [bug-wildfly-enum-getenumconstants-ecj.md](bug-wildfly-enum-getenumconstants-ecj.md) | WildFly | FIXED (`fix/wildfly-enum-constants`) |
| WF-2 | [bug-wildfly-string-format-positional-index.md](bug-wildfly-string-format-positional-index.md) | WildFly | FIXED |
| WF-3 | [bug-wildfly-collections-empty-singleton.md](bug-wildfly-collections-empty-singleton.md) | WildFly | FIXED |
| WF-4 | [bug-wildfly-hashmap-entry-value-cast.md](bug-wildfly-hashmap-entry-value-cast.md) | WildFly | FIXED |
| WF-5 | [bug-wildfly-jaxp-premature-end-of-file.md](bug-wildfly-jaxp-premature-end-of-file.md) | WildFly | OPEN |
| WF-6 | [bug-wildfly-throwable-stack-trace-capture.md](bug-wildfly-throwable-stack-trace-capture.md) | WildFly | OPEN |
| WF-7 | [bug-wildfly-get-field-factory-noise.md](bug-wildfly-get-field-factory-noise.md) | WildFly | OPEN (benign) |
| WF-8 | [bug-wildfly-jar-signer-authenticated-attributes.md](bug-wildfly-jar-signer-authenticated-attributes.md) | WildFly | OPEN (noise) |
| HIB-1 | [bug-hibernate-jpa-persistence-xml-properties.md](bug-hibernate-jpa-persistence-xml-properties.md) | Hibernate | OPEN |
| HIB-2 | [bug-hibernate-duplicate-persistence-unit-scan.md](bug-hibernate-duplicate-persistence-unit-scan.md) | Hibernate | OPEN |
| HIB-3 | [bug-hibernate-log-format-placeholder.md](bug-hibernate-log-format-placeholder.md) | Hibernate | OPEN |
| ES-1 | [bug-elasticsearch-log4j2-serviceloader.md](bug-elasticsearch-log4j2-serviceloader.md) | Elasticsearch | OPEN |
| CM-1 | [bug-commons-math-full-reactor.md](bug-commons-math-full-reactor.md) | Commons Math | OPEN |
| CM-2 | [bug-commons-math-junit-probe-jit-execute.md](bug-commons-math-junit-probe-jit-execute.md) | Commons Math | OPEN |
| BC-PRNG-1 | [bug-bc-crypto-prng-abnormal-exit-127.md](bug-bc-crypto-prng-abnormal-exit-127.md) | Bouncy Castle | OPEN |
| BUILD-1 | [bug-gpu-build-native-builtins-crash.md](bug-gpu-build-native-builtins-crash.md) | GPU build | OPEN |
| SB-G1-1 | [spring-boot-g1-conservative-rootscan-region-lookup-FIXED.md](spring-boot-g1-conservative-rootscan-region-lookup-FIXED.md) | Spring Boot | FIXED (`fix/g1-coldpath-hang`) |

**Apps suite runs:** [`apps/APPS_SUITE_RESULTS.md`](../../apps/APPS_SUITE_RESULTS.md) · [`apps/CRATONVM_CRASHES.md`](../../apps/CRATONVM_CRASHES.md)

**Binary used for 2026-06-05 runs:** `target/release/cratonvm.exe` (fresh CPU build)  
**HotSpot reference:** `C:/Program Files/Java/jdk-25/bin/java.exe`
