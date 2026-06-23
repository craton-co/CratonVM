# Elasticsearch — `CliToolLauncher -V` / `Version.<clinit>` NPE (ES-1)

## Status
**OPEN** on `target/release/cratonvm.exe` (2026-06-05 apps suite).

Log4j2 ServiceLoader / reflective-lambda fix exists on branch `fix/elasticsearch-log4j-serviceloader` but is **not merged** into this binary. This run fails in the **downstream** chain after Log4j2 config begins.

## Severity
**HIGH** — cannot run `org.elasticsearch.launcher.CliToolLauncher -V` on CratonVM.

## App / suite
- **Distro:** `apps/elasticsearch-8.15.5/`
- **Command:** `CliToolLauncher -V` with `-Des.path.home` / `-Des.path.conf`
- **Harness:** `test-infra/run-all-apps-suites.sh`
- **Log:** `test-infra/suite-results/apps-all-20260605-170945/elasticsearch-version-cratonvm.log`

## Symptom

```
ERROR Unable to invoke factory method … LoggersPlugin.createLoggers(…)
  java.lang.NullPointerException: arraylength null
ERROR Unable to invoke factory method … AppendersPlugin … NullPointerException

<clinit> failed — ExceptionInInitializerError class=org/elasticsearch/Version
  cause=NullPointerException: Cannot invoke getClass on null
  [CLINIT-TRACE …] Version.<clinit> (Version.java:208) bci=1934
                   Field.get (Field.java:437)

[cratonvm] System.exit(70)
```

- **rc:** 70 · **wall:** 3.6 s · no version string printed

## HotSpot behavior

Same command prints Elasticsearch version, rc=0.

Use **`-V`** (capital V), not `--version` — ES 8.15.5 rejects positional args on `-- --version`.

## CratonVM behavior — failure chain

| Step | Component | Failure |
|------|-----------|---------|
| 1 | Log4j2 `LoggersPlugin` / `AppendersPlugin` | `@PluginElement` array param is **null** → NPE at `arraylength` (non-fatal ERROR spam) |
| 2 | `org.elasticsearch.Version.<clinit>` | `Field.get(null)` on static field → **NPE: Cannot invoke getClass on null** |
| 3 | `CliToolLauncher.main` | `ExceptionInInitializerError` → `System.exit(70)` |

Minimal `Field.get(null)` on static fields works on CratonVM in isolation — failure is specific to `Version.<clinit>`'s reflection loop (`Build.findLocalBuild` → `Build.current`).

## Root cause (confirmed layers)

1. **Primary (masked early):** Log4j2 loads providers via reflective `LambdaMetafactory` → CratonVM synthetic stub returned unfrozen `ConstantCallSite` → 0 providers → simple logger CCE. **Fixed on branch** `fix/elasticsearch-log4j-serviceloader`.
2. **This run — PluginBuilder null array:** Log4j2 plugin factory receives null instead of empty `LoggerConfig[]` / appenders array.
3. **This run — Version NPE:** static field read during `Version` class init returns/wraps null incorrectly at `Field.get`.

## Reproduce

```bash
ES="apps/elasticsearch-8.15.5"
ES_CP=$(find "$ES/lib" -name '*.jar' | tr '\n' ';')
cratonvm.exe --java-home "<jdk-25>" --Xmx 512m \
  -cp "$ES_CP" -Dcli.name=server \
  "-Des.path.home=$ES" "-Des.path.conf=$ES/config" \
  org.elasticsearch.launcher.CliToolLauncher -V
```

## Fix direction

1. Merge / port Log4j2 ServiceLoader lambda fix.
2. Log4j2 `@PluginElement` array injection — return empty array, not null (`PluginBuilder` / reflection path).
3. Investigate `Version.<clinit>` field iteration vs HotSpot at bci 1934 (`Field.get` on static `Version` fields).

## Related

- Full chain notes: prior sections in git history of this file
- [apps/CRATONVM_CRASHES.md](../../apps/CRATONVM_CRASHES.md)
- Harness: `test-infra/run-three-apps-suite.sh` (`elasticsearch-version`)
