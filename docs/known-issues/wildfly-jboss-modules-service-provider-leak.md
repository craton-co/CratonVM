# WildFly JBoss Modules service-provider lookup leaks across modules

Status: open

Observed while rerunning non-passed WildFly classes on the Azure host with the
JIT-enabled WildFly runner.

## Reproduction

Command shape:

```bash
cd /data/wt/wt-wildfly-nonpassed-20260705-035722/apps/wildfly-suite-runner
CRATONVM_BIN=/data/wt/wt-wildfly-nonpassed-20260705-035722/cratonvm-wildfly-nonpassed-20260705-035722-moddesc \
MVNW=/home/victor/.m2/wrapper/dists/apache-maven-3.9.11/a2d47e15/bin/mvn \
WILDFLY=/data/cratonvm/apps/wildfly JDK25=/home/victor/jdk25 JDK25_WIN=/home/victor/jdk25 \
./run-suite.sh run --category failed --only HostExcludesTestCase --count 1 --jit on --class-to 300 --tag azure-hostexcludes-moddesc-007
```

Failure:

```text
WFLYCTL0226: A subsystem named 'infinispan' cannot be registered by extension
'org.wildfly.extension.core-management' -- a subsystem with that name has
already been registered by extension 'org.jboss.as.jmx'.
```

## Notes

The three relevant WildFly artifacts each contain their own
`META-INF/services/org.jboss.as.controller.Extension` descriptor. CratonVM's
`ModuleClassLoader.findResources` falls back to the process-wide classpath when
a scoped lookup misses, which can expose another module's extension provider to
the current module. Service-provider resources must remain module-scoped.

Follow-up probing showed the native `ServiceLoader` also appended a flat
classpath descriptor scan after the module-scoped lookup, which reintroduced
the same cross-module provider leak.
