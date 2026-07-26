# WildFly JBoss LogManager getEffectiveLevel null loggerNode

Status: ✅ FIXED — verified 2026-07-06, no longer reproduces on `dev`.

Observed while rerunning non-passed WildFly classes on the Azure host after the
JBoss Modules service-provider leak was narrowed.

## Reproduction (original, pre-fix)

Command shape:

```bash
cd /data/wt/wt-wildfly-nonpassed-20260705-035722/apps/wildfly-suite-runner
CRATONVM_BIN=/data/wt/wt-wildfly-nonpassed-20260705-035722/cratonvm-wildfly-nonpassed-20260705-035722-svcjar \
MVNW=/home/victor/.m2/wrapper/dists/apache-maven-3.9.11/a2d47e15/bin/mvn \
WILDFLY=/data/cratonvm/apps/wildfly JDK25=/home/victor/jdk25 JDK25_WIN=/home/victor/jdk25 \
./run-suite.sh run --category failed --only HostExcludesTestCase --count 1 --jit on --class-to 300 --tag azure-hostexcludes-svcjar-014
```

Failure excerpt from `HostExcludesTestCase-output.txt`:

```text
<clinit> failed -- wrapping in ExceptionInInitializerError
class=org/wildfly/security/x500/cert/acme/AcmeClientSpi
cause=java/lang/NullPointerException Cannot invoke
"org.jboss.logmanager.LoggerNode.getEffectiveLevel()" because
"this.loggerNode" is null
```

## Root cause and fix

`native-builtins::logmanager` already had a null-safe
`org.jboss.logmanager.Logger.getEffectiveLevel()I` native that returns the
conservative INFO value, but the real jboss-logmanager bytecode was still
executed for a real-JDK class body, dereferencing a null synthetic
`loggerNode`.

This was fixed in `a3728860` ("Fix WildFly non-passed suite blockers",
2026-07-05), which forces the registered natives over real bytecode for
`org/jboss/logmanager/Logger.getEffectiveLevel()I` and
`isLoggable(Ljava/util/logging/Level;)Z` in both dispatch gates:
- `force_native_over_real_jdk_bytecode` (`../../../../vm/src/runtime/interpreter.rs`)
- `check_override` in `invoke_on_class_shared_inner` (`../../../../vm/src/vm/vm_exec.rs`)

The original repro binary (`cratonvm-wildfly-nonpassed-20260705-035722-svcjar`,
built ~03:45 on 2026-07-05) predates this fix (committed 16:07 the same day),
so the doc was stale rather than describing a live gap.

## Verification (2026-07-06)

Full Maven/WildFly Surefire harness wasn't available on the current Azure
build host, so verification used the real `jboss-logmanager-2.1.19.Final.jar`
and `wildfly-elytron-x500-cert-acme-2.4.2.Final.jar` (plus the rest of the
Elytron/`wildfly-common`/`jboss-logging`/`jakarta.json`+`parsson` dependency
set) extracted directly from `wildfly-32.0.1.Final.zip` — no hand-mocked
stand-ins for the failing classes.

Forcing `Class.forName("org.wildfly.security.x500.cert.acme.AcmeClientSpi", true, ...)`
(the exact class from the failure) under `cratonvm` with `--java-home <real JDK25>`
and JIT on (default) completes cleanly:

```text
INFO [org.wildfly.security] ELY00001: WildFly Elytron version 2.4.2.Final
OK - AcmeClientSpi <clinit> completed without error
```

matching real HotSpot's clinit behavior (also verified with the same
classpath). No NPE, `LoggerNode.getEffectiveLevel()` never dereferences null.

Moved out of `../../../known-issues` per the "only unfixed bugs" convention.
