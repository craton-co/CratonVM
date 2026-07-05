# WildFly: Host Controller fails module load because synthetic Module has null descriptor

## Status

Open as of 2026-07-05 while rerunning WildFly non-passed classes on Azure after fixing the Surefire `PrintStream.write(int)` fork-channel crash.

## Symptom

`HostExcludesTestCase` now reaches WildFly domain startup, but the Host Controller aborts while parsing the host configuration and loading `org.jboss.as.jmx`:

```text
WFLYHC0033: Caught exception during boot
Caused by: javax.xml.stream.XMLStreamException: WFLYCTL0083: Failed to load module org.jboss.as.jmx
Caused by: java.util.concurrent.ExecutionException: java.lang.NullPointerException: Cannot invoke "java.lang.module.ModuleDescriptor.isAutomatic()" because "this.descriptor" is null
```

The test reports a managed-server startup timeout afterwards because the Host Controller process has already exited.

## Evidence

- Host: Azure `victor@20.84.156.31`
- Worktree: `/data/wt/wt-wildfly-nonpassed-20260705-035722`
- Binary: `cratonvm-wildfly-nonpassed-20260705-035722-writeint`
- Run: `apps/wildfly-suite-runner/out/azure-hostexcludes-writeint-006-jit-real-failed-20260705-051151`
- XML/log line: `surefire-reports/00001-org.jboss.as.test.integration.domain.HostExcludesTestCase/TEST-org.jboss.as.test.integration.domain.HostExcludesTestCase.xml:173-197`

## Root-cause hypothesis

The default real-JDK path returns synthetic `java.lang.Module` mirrors for named modules but leaves their real `descriptor` field null. Public access checks are mostly native-overridden, but JDK private module helpers and module-definition code can still read `this.descriptor` directly and then call `ModuleDescriptor.isOpen()` / `isAutomatic()` / collection accessors. The synthetic descriptor support is richer in `register_p59_module` than in the essential real-JDK path, so WildFly can reach real bytecode against a null descriptor.

## Expected fix

Named synthetic modules should carry a non-null synthetic `ModuleDescriptor`, and the essential real-JDK path should register the minimal `ModuleDescriptor` accessors needed by JDK module helper bytecode (`isAutomatic`, `isOpen`, `packages`, `exports`, `opens`, `uses`, `provides`, and related empty-set/optional accessors).
