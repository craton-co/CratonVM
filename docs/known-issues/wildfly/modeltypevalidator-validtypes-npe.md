# WildFly boot: `ModelTypeValidator`/validator-chain `NullPointerException` on `standalone.xml` parse — JIT-only, confirms the `org/jboss/as/` skip-list ban is still live

**Status:** OPEN, unfixed. Confirmed JIT-specific via `--nojit` differential.
Not root-caused to an exact codegen site yet — this doc records the repro and
the `[[jit-regalloc-callee-saved-clobber-family]]`-style symptom signature
for whoever picks it up next.

## Context

This was found while re-testing the app-specific JIT skip-list bans in
`vm/src/jit/skip_list.rs` (the 2026-07-25/26 "jit-ban-sweep" effort) to see
which are safe to lift now that the general callee-saved-GPR-clobber fix
(2026-07-04) and today's JIT/dispatch rework have landed. The `org/jboss/as/` blanket ban
(SPB.8b, added ~2026-05-05, Session 113 r2) was tested by lifting it via
`CRATONVM_JIT_ALLOW_PACKAGES` and booting real WildFly. **Result: it is still
necessary** — lifting it produces a new, clean, fast, deterministic crash.

## Symptom

Direct `standalone.sh` boot (WildFly 32.0.1.Final, real WildFly distribution
at `/data/data/wildfly-dist-keep/wildfly-32.0.1.Final` on the Azure host)
with `CRATONVM_JIT_ALLOW_PACKAGES=org/jboss/modules/,org/jboss/as/,org/wildfly/,org/jboss/msc/,org/jboss/logging/`
(i.e. the SPB.8/8b/8c ban family lifted) boots ~39 threads deep into real
config-model parsing, then:

```
ERROR [org.jboss.as.server] WFLYSRV0055: Caught exception during boot
    org.jboss.as.controller.persistence.ConfigurationPersistenceException: WFLYCTL0085: Failed to parse configuration
        at org.jboss.as.controller.AbstractControllerService$1.run(AbstractControllerService.java:362)
        at org.jboss.as.server.ServerService.boot(ServerService.java:386)
        at org.jboss.as.controller.persistence.XmlConfigurationPersister.load(XmlConfigurationPersister.java:125)
    Caused by: java.lang.NullPointerException: Cannot invoke "java.util.Set.iterator()" because "this.validTypes" is null
        at org.jboss.as.controller.AbstractControllerService$1.run(AbstractControllerService.java:362)
        at org.jboss.as.server.ServerService.boot(ServerService.java:386)
        at org.jboss.as.controller.persistence.XmlConfigurationPersister.load(XmlConfigurationPersister.java:112)
        at org.jboss.staxmapper.XMLMapperImpl.parseDocument(XMLMapperImpl.java:72)
        ...
        at org.jboss.as.server.parsing.StandaloneXml_18.parseServerProfile(StandaloneXml_18.java:649)
        ...
        at org.jboss.as.ee.subsystem.EESubsystemParser60.parseConcurrent(EESubsystemParser60.java:250)
        at org.jboss.as.ee.subsystem.EESubsystemParser60.parseManagedExecutorServices(EESubsystemParser60.java:380)
        at org.jboss.as.ee.subsystem.EESubsystemParser60.parseManagedExecutorService(EESubsystemParser60.java:420)
        at org.jboss.as.controller.SimpleAttributeDefinition.parseAndSetParameter(SimpleAttributeDefinition.java:51)
        at org.jboss.as.controller.AttributeParser.parse(AttributeParser.java:65)
        at org.jboss.as.controller.AttributeParser.parse(AttributeParser.java:82)
        at org.jboss.as.controller.operations.validation.NillableOrExpressionParameterValidator.validateParameter(NillableOrExpressionParameterValidator.java:61)
        at org.jboss.as.controller.operations.validation.LongRangeValidator.validateParameter(LongRangeValidator.java:44)
        at org.jboss.as.controller.operations.validation.ModelTypeValidator.validateParameter(ModelTypeValidator.java:143)
FATAL [org.jboss.as.server] WFLYSRV0056: Server boot has failed in an unrecoverable manner; exiting.
```

`validTypes` is a `Set<ModelType>` field on `ModelTypeValidator`, populated by
its own constructor(s) (upstream `wildfly-core` `ModelTypeValidator`, several
overloaded ctors all eventually call a common `this(...)` chain that sets
`this.validTypes = EnumSet.copyOf(types)` or similar). The field reading as
`null` when `validateParameter` runs means the constructor's store to that
field either didn't execute or didn't become visible — the classic
allocate-then-putfield JIT miscompile signature already catalogued across
~25 other bans in this same file (see the module's own SPB.1-9d comments,
all citing the identical pattern).

Note: CratonVM's own stack unwinding attributes several frames of this trace
to `AbstractControllerService$1.run` / `ServerService.boot` that don't match
the real upstream call chain shape — likely an inlined-callee frame
mis-attribution (same class of imprecision noted in JASPER-JDT.3's writeup),
not necessarily meaningful for root-causing; trust the *leaf* frames
(`ModelTypeValidator.validateParameter` etc.) over the top of the trace.

## Confirmed JIT-specific (differential)

| Config | Result |
|---|---|
| Ban in place (no `CRATONVM_JIT_ALLOW_PACKAGES`) | **Boots cleanly**: `WFLYSRV0025: WildFly Full 32.0.1.Final ... started in 33227ms`, HTTP management up, deployment scanner running normally. |
| Ban lifted, JIT on | **FATAL WFLYSRV0056**, NPE above, ~10-20s into boot. |
| Ban lifted, `--nojit` added | **Boots cleanly**: `WFLYSRV0025: ... started in 24042ms`, same as baseline. |

The `--nojit` result rules out an environment/harness issue (fake-JDK-home
`bin/java` shape, `CRATONVM_JAVA_HOME`, missing `configuration/` dir, etc.) —
those would fail identically with or without JIT. Only the JIT-on + ban-lifted
combination fails.

## Reproduction

```bash
mkdir -p /data/tmp/wf-javahome/bin
cp <cratonvm-binary> /data/tmp/wf-javahome/bin/java
chmod +x /data/tmp/wf-javahome/bin/java
mkdir -p /data/tmp/wf-run && cp -r /data/data/wildfly-dist-keep/wildfly-32.0.1.Final/standalone/configuration /data/tmp/wf-run/
JAVA_HOME=/data/tmp/wf-javahome CRATONVM_JAVA_HOME=/home/victor/jdk25 \
  CRATONVM_JIT_ALLOW_PACKAGES='org/jboss/modules/,org/jboss/as/,org/wildfly/,org/jboss/msc/,org/jboss/logging/' \
  timeout 90 bash /data/data/wildfly-dist-keep/wildfly-32.0.1.Final/bin/standalone.sh \
  -Djboss.server.base.dir=/data/tmp/wf-run
```
(`standalone.sh` isn't `chmod +x` in the dist — always invoke via `bash standalone.sh`.
See the WildFly GC-barrier boot-hang-and-harness-fixes writeup (since archived)
for why the fake-JDK-home + `CRATONVM_JAVA_HOME` shape is required for the
managed/direct-boot case to actually exercise CratonVM as the server JVM.)

Takes ~10-20s to hit the NPE — much faster than the full Arquillian-managed
test-suite harness, useful as a fast bisection loop
(`CRATONVM_JIT_BISECT_ONLY=org/jboss/as/controller/operations/validation/`,
narrowing further with `CRATONVM_JIT_BISECT_SKIP=<Class>.<method>`) for
whoever root-causes this.

## Disposition

**Do not lift the `org/jboss/as/` (SPB.8b) ban** in `vm/src/jit/skip_list.rs`
until this is root-caused and fixed — it is still live, not stale. The
sibling bans in the same lift set (`org/jboss/modules/`, `org/wildfly/`,
`org/jboss/msc/`, `org/jboss/logging/`) were lifted together in this test, so
individually they are NOT yet confirmed either-way — worth re-testing each
alone (or in smaller combinations) once this issue is fixed, since the boot
may now get further and expose (or clear) different bans downstream.

## Related

- The general callee-saved-GPR-local-homes fix (2026-07-04, since archived)
  this symptom family was supposed to close; this repro shows at least one
  member of the family (validator-object field population under JIT) is not
  fully closed.
- Found during the 2026-07-25/26 "jit-ban-sweep" session effort (see Context
  above).
