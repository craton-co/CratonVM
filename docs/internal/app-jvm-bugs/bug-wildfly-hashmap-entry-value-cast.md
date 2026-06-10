# WildFly — `HashMap.values()` mis-identifies `*Entry` value types as map entries

## Status
**FIXED** on `fix/wildfly-enum-constants` (`native-collections/src/lib.rs`, commit `ec9aa43`).

## Severity
**FATAL for standalone boot** — `System.exit(1)` during PathManager service start.

## App / suite
- **Daemon:** `org.jboss.as.standalone`
- **Service:** `PathManagerService.addPathManagerResources`
- **Logs:** wildfly-daemon suite logs

## Symptom (before fix)

```
java.lang.ClassCastException: java/util/HashMap$Entry cannot be cast to
  org/jboss/as/controller/services/path/PathEntry
WFLYSRV0239: … exit code 1
```

Bytecode pattern: `pathEntries.values().iterator().next()` + `checkcast PathEntry` where map is `HashMap<String, PathEntry>`.

## HotSpot behavior

`values()` returns a collection of **`PathEntry`** instances.

## CratonVM behavior (before fix)

CratonVM `HashMap`/`TreeMap` **values()** and **entrySet()** use ArrayList-backed live views. When resyncing a values view from an entrySet view, code tested head element class name with **`contains("Entry")`**.

Application type **`PathEntry`** matches → view rebuilt as synthetic **`java/util/HashMap$Entry`** objects (`tm_make_entry`) → cast fails.

Any value class whose simple name contains `"Entry"` was affected.

## Root cause (confirmed)

Over-broad heuristic confused app value types with JDK-internal map entry node types.

## Fix

`is_synthetic_map_entry_class` matches only **`java/util/*$Entry`** (sealed package — apps cannot collide).

**Verified:** HashMap/TreeMap values+entrySet+remove with `*Entry` value type 6/6 vs HotSpot. Standalone boots past PathManager to `WFLYSRV0049`.

## Reproduce

```bash
CV=target-bench/release/cratonvm.exe
WF=test-infra/regression-pool/apps/wildfly-32.0.1.Final
"$CV" --java-home "C:/Program Files/Java/jdk-25" -Xmx1g \
  -Djboss.home.dir="$WF" --jar "$WF/jboss-modules.jar" \
  -- -mp "$WF/modules" org.jboss.as.standalone
```

## Related

- `apps/wildfly/CRATONVM_BUGS.md` Bug 2
