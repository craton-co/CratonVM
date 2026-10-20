# Hibernate ORM Suite — Single-JVM Execution Failures and Process Exit Analysis

**Date:** 2026-10-10  
**Host:** Azure Linux (`azureuser@20.80.105.49`, JDK 25 Adoptium `/data/jdkimages/jdk25-linux/jdk-25.0.4+7`)  
**Runner Directory:** `/data/CVM/apps/hib-suite-runner`  
**Target Suite:** Hibernate ORM Core (4,282 compiled test classes)

---

## 1. Executive Summary

During full single-JVM process execution of the 4,282 compiled Hibernate ORM test classes on CratonVM, the single VM process executed 1,543 test classes continuously in 574.4 seconds before process termination on class 1544 (`JsonTableTest`).

- **Total Classes Evaluated:** 1,543
- **PASS:** 1,316 classes (85.3% pass rate among executed classes)
- **FAIL:** 173 classes
- **NOTESTS:** 54 classes
- **ABORTED:** 0 classes
- **Process Exit / Crash:** Triggered at class 1544 due to a JUnit Platform jar version alignment check.

---

## 2. Database Environment Used

- **Default Database Engine:** H2 In-Memory Database (`jdbc:h2:mem:...`) is the default fallback engine used by Hibernate ORM when no external JDBC URL or dialect property is specified on the command line.
- **Root Cause of 111 Database Failures:**  
  111 of the 173 failures were thrown with:
  ```
  org.hibernate.engine.jdbc.connections.spi.ConnectionProviderConfigurationException: No JDBC URL specified by property 'jakarta.persistence.jdbc.url'
  ```
  These integration test families (e.g. `org.hibernate.orm.test.action.queue.*`) explicitly mandate an external database connection URL (such as PostgreSQL / MySQL docker container URL) via `-Djakarta.persistence.jdbc.url=...`, rather than falling back to H2 in-memory mode.

---

## 3. Failure Breakdown (173 Failed Classes)

| Failure Category | Class Count | Primary Error Signature / Root Cause |
| :--- | :---: | :--- |
| **DB / Connection / Dialect** | **111** | `ConnectionProviderConfigurationException: No JDBC URL specified by property 'jakarta.persistence.jdbc.url'` |
| **Assertion Failures** | **20** | `org.opentest4j.AssertionFailedError` / `org.hibernate.testing.junit4.ExtraAssertions` |
| **StAX DTD Schema Lookup** | **12** | `XmlInfrastructureException: Unable to locate schema [org/hibernate/hibernate-mapping-3.0.dtd] via classpath` |
| **Missing CDI Provider** | **8** | `IllegalStateException: No valid CDI implementation found` (Weld / SmallRye CDI container dependency) |
| **Bytecode / Enhancement ClassCast** | **5** | `ClassCastException: class org.hibernate.orm.test.bytecode.enhancement...` |
| **Mapping / Resource Not Found** | **4** | `MappingNotFoundException: Mapping (RESOURCE) not found` |
| **Cache / Configuration / Other** | **13** | `CacheException`, `ConfigurationException`, or `TestAbortedException` |

---

## 4. Root Cause Analysis of Process Exit (Class 1544)

The single-process run terminated at test class 1,544 (`org.hibernate.orm.test.function.json.JsonTableTest`) with the following stack trace:

```text
@@CLASSFAIL org.hibernate.orm.test.function.json.JsonTableTest org.junit.platform.commons.JUnitException: 
The wrapped NoClassDefFoundError is likely caused by the versions of JUnit jars on the classpath/module path not being properly aligned.
The following conflicting versions were detected:
- org.junit.jupiter.api: 6.1.3
- org.junit.jupiter.engine: 6.1.3
- org.junit.jupiter.params: 6.1.3
- org.junit.platform.commons: 6.1.3
- org.junit.platform.engine: 6.1.3
- org.junit.platform.launcher: 6.1.3
- org.junit.platform.suite.api: 6.1.2
- org.junit.platform.suite.engine: 6.1.2
- org.junit.vintage.engine: 6.0.3

[cratonvm] main-vm run() returned Err: Exception in thread "main" org.junit.platform.commons.PreconditionViolationException: No tests have yet been executed
	at org.junit.platform.commons.util.Preconditions.condition(Preconditions.java:373)
	at org.junit.platform.commons.util.Preconditions.notNull(Preconditions.java:57)
	at org.junit.platform.launcher.listeners.SummaryGeneratingListener.getMutableSummary(SummaryGeneratingListener.java:54)
	at org.junit.platform.launcher.listeners.SummaryGeneratingListener.getSummary(SummaryGeneratingListener.java:50)
	at CratonRunner.main(CratonRunner.java:71)
```

### **Mechanism of Failure:**
1. **Multi-Version Classpath Pollution:** The initial classpath generation script recursively collected all `.jar` files in `~/.gradle/caches/modules-2/files-2.1/` (917 total JARs), resulting in multiple conflicting versions of JUnit Platform (`org.junit.platform.suite.engine:6.1.2` alongside `org.junit.platform.engine:6.1.3`).
2. **JUnit 6 Version Alignment Enforcement:** When `JsonTableTest` loaded the Suite engine, JUnit 6's internal `UniqueIdTrackingListener` threw a `NoClassDefFoundError` wrapped in `JUnitException` due to the minor-version mismatch (6.1.2 vs 6.1.3).
3. **Uncaught Precondition Exception:** The resulting error prevented test execution for that class, causing `SummaryGeneratingListener.getSummary()` inside `CratonRunner.main` to throw an uncaught `PreconditionViolationException` ("No tests have yet been executed"), which exited the JVM process.

---

## 5. Resolution and Prevention

1. **Classpath Deduplication (`build_clean_cp.py`):**
   - Implemented version-aware JAR deduplication when parsing `~/.gradle/caches/modules-2/files-2.1/`.
   - Reduced raw classpath entries from 917 down to **599 deduplicated entries**, selecting the newest version for each library key and eliminating JUnit version collisions.
2. **CratonRunner Exception Isolation:**
   - Wrapped `launcher.execute(request)` in a `try-catch (Throwable t)` block inside `CratonRunner.main` so an unhandled test discovery exception in a single class prints `@@CLASSFAIL <className> <exception>` and proceeds to the remaining classes without terminating the JVM.
3. **List-File Argument Support:**
   - Updated `CratonRunner` to support reading class lists directly via `@file.list` syntax, bypassing OS command-line argument length constraints (`E2BIG`).
