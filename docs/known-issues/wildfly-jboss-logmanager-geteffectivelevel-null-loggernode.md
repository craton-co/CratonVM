# WildFly JBoss LogManager getEffectiveLevel null loggerNode

Status: open

Observed while rerunning non-passed WildFly classes on the Azure host after the
JBoss Modules service-provider leak was narrowed.

## Reproduction

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

## Notes

`native-builtins::logmanager` already has a null-safe
`org.jboss.logmanager.Logger.getEffectiveLevel()I` native that returns the
conservative INFO value. The failure shows the real jboss-logmanager bytecode
was still executed for a real-JDK class body, dereferencing a null synthetic
`loggerNode`. The fix should force that registered native over the real bytecode
