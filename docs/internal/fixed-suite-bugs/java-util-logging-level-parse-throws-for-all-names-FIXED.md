# FIXED: `java.util.logging.Level.parse(String)` threw `IllegalArgumentException` for every name

Fixed: 2026-07-10, branch `fix/wildfly-hib32-residuals-20260710`
Found while investigating: `docs/known-issues/wildfly-domain-heap-corrupt-value-timeout.md`

## Symptom

Standalone repro, no app server involved:

```java
import java.util.logging.Level;
public class MyLevelProbe {
    public static void main(String[] a) throws Exception {
        System.out.println(Level.parse("WARNING")); // a STANDARD JDK constant
    }
}
```

threw, under real-JDK mode:

```text
Exception in thread "main" java/lang/IllegalArgumentException: Bad level "WARNING"
	at MyLevelProbe.main(MyLevelProbe.java:4)
	at java/util/logging/Level.parse(Level.java:525)
```

This is not limited to JBoss LogManager's extended level names
(`WARN`/`ERROR`/`FATAL`/`DEBUG`/`TRACE`) — even the 9 built-in
`java.util.logging.Level` constants (`OFF`/`SEVERE`/`WARNING`/`INFO`/
`CONFIG`/`FINE`/`FINER`/`FINEST`/`ALL`) failed to parse by name. Constructing
a `Level` object directly, or reading `Level.WARNING` as a static field,
both worked fine — only the string-name lookup (`Level.parse(String)`) was
broken.

This surfaced concretely as WildFly's `host.xml`/`domain.xml` parser failing
with `WFLYLOG0026: Log level WARN is invalid` on the very first
`<level name="WARN"/>` element in a stock (unmodified) config — before
`docs/known-issues/wildfly-domain-heap-corrupt-value-timeout.md`'s own
front-line residuals could ever be reached.

## Root cause

Real JDK 25's `Level.parse(String)` resolves names through
`Level$KnownLevel.findByName`, whose internal implementation involves a
`ClassLoaderValue`-keyed lookup that needs a non-null `java.lang.Module` for
some class/classloader in the resolution chain. CratonVM's module-system
synthesis is incomplete for this path (the same class of gap already tracked
in `docs/internal/gaps/kc16-blocker-map.md`'s KC16 investigation — that
session hit an identical mechanism via a different call site,
`JDKModuleLogger.<clinit>`, and worked around it with a narrow `<clinit>`
stub rather than fixing `Class.getModule()` synthesis generally). The lookup
throws an internal `NullPointerException` ("Cannot invoke isNamed on null"),
which `Level.parse`'s own code silently treats as "no match found," falling
through to the generic `throw new IllegalArgumentException("Bad level \"" +
name + "\"")` at the end of the method — regardless of whether the
requested name was a genuine standard constant or a legitimate custom
extension.

Note: `Level` object *construction* (`new Level(name, value)`, and the
static `SEVERE`/`WARNING`/etc. fields set during `Level`'s own `<clinit>`)
works correctly — the broken step is specifically the *string-name lookup*
side (`KnownLevel.findByName`), not the *registration* side
(`KnownLevel.add`, called from the constructor).

## Fix

Added a native override for `java.util.logging.Level.parse(String)`
(`native_level_parse`, `native-builtins/src/logmanager.rs`) that bypasses the
broken `KnownLevel` registry lookup entirely, mirroring the existing
workaround already used for the adjacent
`org.jboss.logmanager.LogContext.getLevelForName`
(`native_jboss_log_context_get_level_for_name`, same file — this bug's
root cause is the same one that function was already built to route around,
just not for this JDK-native entry point):

1. Check the 9 standard `java.util.logging.Level` static fields by
   upper-cased name.
2. Check `org.jboss.logmanager.Level`'s 5 extension fields
   (`FATAL`/`ERROR`/`WARN`/`DEBUG`/`TRACE` — `INFO` is intentionally
   omitted since it aliases the standard constant already checked first),
   if that class happens to be resolvable.
3. Numeric fallback: parse the name as an integer and scan both classes'
   known constants for an exact `intValue()` match; if none matches,
   synthesize a fresh, unnamed `Level` via `new_object_initialized`
   (construction is not affected by this bug, only lookup).
4. Otherwise throw the real `IllegalArgumentException("Bad level \"" +
   name + "\"")`, matching the genuine JDK contract for truly unknown names.

Forced to win over real bytecode via `force_native_over_real_jdk_bytecode`
(`vm/src/runtime/interpreter.rs`) — `Level.parse` is invoked via
`invokestatic`, which (unlike `invokespecial`) defaults to preferring real
bytecode unless explicitly force-listed; without this, the native is never
consulted despite being registered.

## Verification

Standalone repros (`CRATONVM_JAVA_HOME=<real JDK 25>`, no app server):

```text
Level.parse("WARNING") -> WARNING   (was: IllegalArgumentException)
Level.parse("WARN")    -> WARN      (org.jboss.logmanager.Level; was: IllegalArgumentException)
```

WildFly `domain.sh` boot against a pristine WildFly 32.0.1.Final
distribution: the `WFLYLOG0026: Log level WARN is invalid` /
`WFLYCTL0085: Failed to parse configuration` failure during `host.xml`
parsing no longer occurs.

## Not covered by this fix

- `Level.parse`'s localized-resource-bundle-name matching path (a rarely-used
  real-JDK feature) is not implemented in the native override — falls
  through to the numeric/unknown-name paths, same as before this fix for
  that specific sub-case.
- The underlying `Class.getModule()`/module-synthesis gap this bug is a
  symptom of (per `docs/internal/gaps/kc16-blocker-map.md`) is NOT fixed —
  only this one call site is routed around. Any *other* JDK code that
  independently depends on `KnownLevel`-style module-keyed
  `ClassLoaderValue` caching will still hit the same underlying gap.
