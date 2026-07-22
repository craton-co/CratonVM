# SBR-14 — Custom `URLClassLoader(parent=null)` bypassed; class reported loaded by `AppClassLoader`

**Status:** 🔴 Open — **correctness / classloader isolation** (CratonVM-only).
**Recommendation:** **HANDOFF** — classloader-delegation semantics; framework-impacting.
**Binary:** `cvsbfull.exe` (dev `df11ac00`) vs HotSpot `jdk-25`.

## Affected probe

`UCLProbe`.

## Symptom

The probe builds a `URLClassLoader` over `runner/` with **`parent = null`** to
force loading through the child, then loads `MProbe$Base`:

```java
try (URLClassLoader ucl = new URLClassLoader(new URL[]{ runnerDirUrl }, null)) {
    Class<?> c = ucl.loadClass("MProbe$Base");
    System.out.println("loaded: " + c + " loader=" + c.getClassLoader());
}
```

```
CratonVM: loaded: class MProbe$Base loader=jdk.internal.loader.ClassLoaders$AppClassLoader@HASH
HotSpot:  loaded: class MProbe$Base loader=java.net.URLClassLoader@HASH
```

CratonVM resolves the class through the **application** class loader and reports
`AppClassLoader` as the defining loader; HotSpot correctly defines it in the
**custom `URLClassLoader`**. So the custom loader's delegation (parent=null →
bootstrap only, then its own URLs) is **not honored** — the app loader services
the request instead.

## Root cause (hypothesis)

CratonVM's class resolution does not route `URLClassLoader.loadClass` through the
custom loader's URL search when `parent == null`; it falls back to the system/app
loader (likely a global class lookup that ignores the requesting loader's
delegation chain). The **defining loader** recorded on the resulting `Class` is
therefore wrong.

This breaks **classloader isolation** — the cornerstone of plugin systems,
Gradle/Spring Boot nested-jar loaders, and parent-last/child-first loaders.

## Repro

```bash
cd C:/craton/CratonVM/apps/spring-boot/buildSrc
CP="runner;$(cat test-classpath.txt)"
"C:/craton/CratonVM-sbfull/target/release/cvsbfull.exe" --java-home "C:/Program Files/Java/jdk-25" -cp "$CP" UCLProbe
"C:/Program Files/Java/jdk-25/bin/java.exe" -cp "$CP" UCLProbe
```

## Impact

High for any framework relying on isolated/child-first class loaders (Spring Boot
`LaunchedClassLoader`, Gradle worker loaders, plugin containers). Classes load
from the wrong loader → wrong static state, `ClassCastException` across loaders,
broken isolation. Deep semantics → handoff.
