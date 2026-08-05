# `TomcatReactiveWebServerFactoryTests.whenServerIsShuttingDownGracefullyThenNewConnectionsCannotBeMade` — 30s awaitility timeout

**Status: OPEN — found 2026-08-05, likely load-induced flake, not yet confirmed**

## Symptom

**1/46 tests fail:**

```
org.awaitility.core.ConditionTimeoutException: Condition with Lambda expression in org.springframework.boot.tomcat.reactive.TomcatReactiveWebServerFactoryTests was not fulfilled within 30 seconds.
  ...
  org.springframework.boot.tomcat.reactive.TomcatReactiveWebServerFactoryTests.whenServerIsShuttingDownGracefullyThenNewConnectionsCannotBeMade(TomcatReactiveWebServerFactoryTests.java:267)
Caused by: java.util.concurrent.TimeoutException
```

The `.err.log` for the same run also shows two `NoSuchMethodError`s
(`java/util/HashMap.getContents()[[Ljava/lang/Object;`,
`java/util/HashMap.handleGetObject(Ljava/lang/String;)Ljava/lang/Object;`,
both attributed to `org/apache/tomcat/util/res/StringManager.getString`)
logged as `WARN`, not thrown — these look like Tomcat's `StringManager`
probing for an optional `ListResourceBundle`-shaped method on a plain
`java/util/HashMap`-typed message-source object and gracefully falling
back when it's absent, matching the pattern in the already-fixed
`docs/internal/fixed-suite-bugs/CRATONVM_BUGS/BUG-J-resourcebundle-native-shadows-subclass.md` /
`BUG-L-resourcebundle-locale-resolution.md` family. Not confirmed to be
related to the test failure (a caught, logged warning, not the exception
that failed the test) — flagged in case it recurs as a hard failure
elsewhere.

## Cross-check

HotSpot baseline passes cleanly: `TomcatReactiveWebServerFactoryTests`
46/46 in 29.6s (`hotspot-baseline-latest.tsv` row 132). Not a CRLF-fixture
issue. No existing doc found for this exact test method or timeout shape.

## Assessment

A related graceful-shutdown test in the same problem family
(`whenARequestIsActiveAfterGracefulShutdownEndsThenStopWillComplete`, a
different method, in `NettyReactiveWebServerFactoryTests`) was explicitly
characterized as a **load-induced flake** in the 2026-08-04
`sslsocketfactory-getdefault-aether-resolution-regression` doc's A/B
table ("three interleaved A/B repeats give `failed=1` on both binaries
every time"), and this investigation independently found the shared Azure
host running many concurrent `cratonvm`/`cargo build` processes from other
sessions at the time of this run. A 30s condition timeout (vs. a 300s hard
process timeout) is small enough that host contention delaying Tomcat's
internal connector shutdown bookkeeping is a plausible, unconfirmed
explanation. Needs a clean-host rerun (or 3+ interleaved repeats against a
HotSpot control, per this repo's own A/B protocol) before concluding this
is a genuine CratonVM timing bug rather than noise.

## Affected classes

- `module/spring-boot-tomcat` — `org.springframework.boot.tomcat.reactive.TomcatReactiveWebServerFactoryTests` (`whenServerIsShuttingDownGracefullyThenNewConnectionsCannotBeMade`)
