# WildFly Module.loadService sees a null moduleClassLoader

Status: open

Observed while rerunning `HostExcludesTestCase` on the Azure WildFly runner
after scoping JBoss Modules service-provider resources.

## Reproduction

Run:

```bash
./run-suite.sh run --category failed --only HostExcludesTestCase --count 1 --jit on --class-to 300 --tag azure-hostexcludes-svcscope-008
```

Failure:

```text
WFLYCTL0083: Failed to load module org.jboss.as.jmx
```

## Notes

`org.jboss.modules.Module.loadService(Class)` reads the real
