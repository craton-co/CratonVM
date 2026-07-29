# Reproducers — Keycloak RUNTIME_INIT investigation, 2026-07-28

Written while fixing
`docs/internal/keycloak/keycloak-no-vertx-http-runtime-init-20260728.md`.
Each probe prints `== DONE OK ==` on success and is meant to be diffed against
HotSpot.

## Framework-independent (these are the regression-worthy ones)

| probe | bug it isolates | classpath |
|---|---|---|
| `CallerBundleProbe.java` + `BundleUser.java` | `Class.getModule()` reported the app loader for a class defined by a custom loader, so the caller-sensitive `ResourceBundle.getBundle(String)` searched the wrong classpath | `-cp .` (it builds its own child loader) |
| `PropsProbe.java` | `Properties.load` + both `getProperty` overloads | `-cp .` |
| `JulLevelProbe.java` | `java.util.logging` effective-level / `isLoggable` inheritance | `-cp .` |

`CallerBundleProbe` must be run with `BundleUser.class` present on the
classpath (it copies the bytes into a temp dir and loads it from a child
loader, so the child — not the app loader — is the defining loader):

```bash
javac -d . CallerBundleProbe.java BundleUser.java
java -cp . CallerBundleProbe
```

Expected (HotSpot and a fixed CratonVM):

```
BundleUser module=unnamed module @... moduleLoader=java.net.URLClassLoader@...
caller-sensitive getBundle -> OK k=v bundleLoaderVisible=true
```

## jboss-logging

`JbossLogProbe.java` — level gating plus the printf/`MessageFormat` rendering
of `doLogf`/`doLog`. Needs the Keycloak dist jars:

```bash
CP=$(ls <dist>/lib/lib/main/*.jar <dist>/lib/lib/boot/*.jar | tr '\n' ':').
javac -cp "$CP" JbossLogProbe.java && java -cp "$CP" JbossLogProbe
```

The INFO line must render as
`INFO-fmt d=42 s=str b=true f=3.14 x=ff c=Z pct=%` — before the fix CratonVM
printed `d=%d s=42 b=%b f=%.2f x=%x c=%c`, i.e. unsupported conversions leaked
AND shifted every later argument.

## Quarkus fast-jar / `RunnerClassLoader`

These build the REAL `RunnerClassLoader` from the dist's
`lib/quarkus/quarkus-application.dat`, so they need only the `lib/lib/boot`
jars on `-cp` and reach everything else through the runner loader — which is
exactly the loader-visibility asymmetry the bugs lived in.

| probe | what it checks |
|---|---|
| `RunnerBundleProbe.java` | `ResourceBundle` through the runner loader; `liquibase.lockservice.StandardLockService.<clinit>` is the load-bearing assertion (`clinit OK`) |
| `AntlrLoaderProbe.java` | `org.antlr.v4.runtime.atn.ATNConfig` and friends resolve through the runner loader |
| `IspnProbe.java` | Infinispan `Version` + the `ConfigurationParser` ServiceLoader |
| `IspnParseProbe.java` | full `ParserRegistry.parse(cache-local.xml)` |
| `EqeProbe.java` | `org.jboss.threads.EnhancedQueueExecutor` builds and runs a task |

Note that `IspnProbe` / `IspnParseProbe` / `RunnerBundleProbe` all PASSED on
CratonVM while the real boot still failed: the boot-only variables were GC
pressure (the `Properties` side-table capacity drop) and >10 000 live
`Properties` objects. An isolated probe passing is not evidence the surface is
healthy under a real boot — see the write-up.

The paths inside these probes are hardcoded to
`/data/tmp/kc-dist/keycloak-26.6.1`; edit them for another host.
