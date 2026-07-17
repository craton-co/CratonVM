# `ServletComponentScanIntegrationTests` — one expected component missing from scan

**Status: OPEN — found 2026-07-17, not root-caused**

## Symptom

Module `module/spring-boot-web-server`, class `ServletComponentScanIntegrationTests`, test `componentsAreRegistered()`:

```
java.lang.AssertionError:
Expected size: 2 but was: 1 in:
{"org.springframework.boot.web.server.servlet.context.testcomponents.servlet.TestServlet"=Mock for Dynamic, hashCode: 368161}
```

Only the `TestServlet` component was registered; a second expected component (likely a listener or filter from the same `testcomponents` scan package) is missing.

Log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard*/logs/module_spring-boot-web-server.ServletCompo-f17c801cca8c.out.log` (lines 17-24).

This class is also listed as originally-affected in
[`../../internal/springboot/tomcatservletwebserverfactory-cross-module-classnotfound-crash-FIXED.md`](../../internal/springboot/tomcatservletwebserverfactory-cross-module-classnotfound-crash-FIXED.md),
but its current failure signature (a component-count assertion) is clearly
unrelated to that doc's Tomcat-hardcoded-shim fix — this is an independent,
newer issue that surfaced once the fatal crash stopped masking it.

## Root cause

**Not root-caused.** Neither the `ServletComponentScanIntegrationTests.java`
source nor the servlet-component-registrar's CratonVM-side scan path was
read yet. Needs a follow-up to identify which second component is expected
(check the `testcomponents` package for a `@WebListener`/`@WebFilter` sibling
of `TestServlet`) and why CratonVM's `ServletComponentScan` registration
finds only one.

## Affected classes

- `module/spring-boot-web-server` | `ServletComponentScanIntegrationTests`
