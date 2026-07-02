# WildFly Surefire JUnit4 Description Linkage Failure

Status: open

Date observed: 2026-07-01

## Summary

The WildFly testsuite runner reaches the forked Surefire JVM under CratonVM
`jit-real`, but Surefire exits before running any test method because CratonVM
throws a linkage error while Surefire's JUnit4 provider reflects
`org.junit.runner.Description`.

Observed failure:

```text
NoSuchMethodError method="org/junit/runner/Description.createSuiteDescription(Ljava/lang/String;)Lorg/junit/runner/Description;"
caller="org/apache/maven/surefire/common/junit4/JUnit4Reflector.createDescription(Ljava/lang/String;)Lorg/junit/runner/Description; @pc=4"
[SUREFIRE-RUN] execute threw: InternalError(Linkage(NoSuchMethodError { class_name: "org/junit/runner/Description", method_name: "createSuiteDescription", method_descriptor: "(Ljava/lang/String;)Lorg/junit/runner/Description;" }))
```

Surefire then reports:

```text
The forked VM terminated without properly saying goodbye. VM crash or System.exit called?
Process Exit Code: 1
```

## Repro

Prerequisite used in this session:

```bash
cd /c/craton/CratonVM/apps/wildfly
./mvnw -B -ntp -pl boms/standard-ee,boms/standard-test,testsuite/shared -am install -DskipTests
./mvnw -B -ntp -pl boms/common-ee,boms/standard-preview-ee,boms/standard-legacy-ee -am install -DskipTests
./mvnw -B -ntp -pl messaging-activemq/injection,messaging-activemq/subsystem,testsuite/build-demander-base,testsuite/build-demander-expansion -am install -DskipTests
```

Runner repro:

```bash
cd /c/craton/CratonVM
"C:/Program Files/Git/bin/bash.exe" apps/wildfly-suite-runner/run-suite.sh run --category all --count 1 --class-to 900 --tag verify-jit-on
```

Result:

```text
out/verify-jit-on-jit-real-all-20260701-182956/results.tsv
org.jboss.as.test.integration.domain.DefaultConfigSmokeTestCase  CRASH
```

Full log:

```text
apps/wildfly-suite-runner/out/verify-jit-on-jit-real-all-20260701-182956/logs/00001-org.jboss.as.test.integration.domain.DefaultConfigSmokeTestCase.log
```

## Notes

The runner uses a Surefire-compatible `java.exe` shim by copying
`target/release/cratonvm.exe` to `out/.../wrappers/jit-real/bin/java.exe`.
The forked process receives `CRATONVM_JAVA_HOME` pointing at Microsoft JDK
25.0.3.9.

This is not a Maven setup failure: after the WildFly prerequisite artifacts were
installed locally, Maven reached Surefire and CratonVM entered
`ForkedBooter.run`.
