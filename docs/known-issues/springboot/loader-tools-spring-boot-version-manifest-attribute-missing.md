# `Spring-Boot-Version` manifest attribute silently missing from packaged jars

**Status: OPEN — found 2026-07-17**

## Symptom

| Module | Class | Method | Wall time |
|---|---|---|---:|
| `loader/spring-boot-loader-tools` | `ImagePackagerTests` | `springBootVersion()` | 13.2s (1/37 fail) |
| `loader/spring-boot-loader-tools` | `RepackagerTests` | `springBootVersion()` | 49.7s (part of 9/52 fail) |
| `loader/spring-boot-loader-tools` | `RepackagerTests` | `jarIsOnlyRepackagedOnce()` | (same run, same 9/52) |

```
JUnit Jupiter:ImagePackagerTests:springBootVersion()
  => java.lang.AssertionError:
Expecting actual:
  {Spring-Boot-Classpath-Index="BOOT-INF/classpath.idx", Start-Class="a.b.C", Spring-Boot-Classes="BOOT-INF/classes/", Spring-Boot-Lib="BOOT-INF/lib/", Manifest-Version="1.0", Main-Class="org.springframework.boot.loader.launch.JarLauncher"}
to contain key:
  Spring-Boot-Version
       org.springframework.boot.loader.tools.AbstractPackagerTests.springBootVersion(AbstractPackagerTests.java:388)
```

`RepackagerTests.springBootVersion()` fails identically (same assertion, same
missing key). `RepackagerTests.jarIsOnlyRepackagedOnce()` fails as a direct
downstream consequence (see Root cause):

```
JUnit Jupiter:RepackagerTests:jarIsOnlyRepackagedOnce()
  => org.opentest4j.AssertionFailedError:
expected: "a.b.C"
 but was: "org.springframework.boot.loader.launch.JarLauncher"
       org.springframework.boot.loader.tools.RepackagerTests.jarIsOnlyRepackagedOnce(RepackagerTests.java:84)
```

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/loader_spring-boot-loader-tools.org.springframework.boot.loader.tools.ImagePackagerTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/loader_spring-boot-loader-tools.org.springframework.boot.loader.tools.RepackagerTests.out.log`

(`RepackagerTests` has 6 additional, unrelated failures in the same run —
see `loader-tools-manifest-entries-and-zip-fidelity-residuals.md`.)

## Root cause

**Grounded in real source on both the Java and CratonVM sides; the
specific reason `getImplementationVersion()` returns null in this test
environment is a plausible but unconfirmed hypothesis.**

`Packager.write()` (`apps/spring-boot/loader/spring-boot-loader-tools/src/main/java/org/springframework/boot/loader/tools/Packager.java:397`) writes the attribute unconditionally:

```java
attributes.putValue(BOOT_VERSION_ATTRIBUTE, getClass().getPackage().getImplementationVersion());
```

where `BOOT_VERSION_ATTRIBUTE = "Spring-Boot-Version"` (`Packager.java:66`).
`getClass()` here is `Packager` itself, so this resolves to
`Packager.class.getPackage().getImplementationVersion()`.

`Class.getPackage()` is a real native in CratonVM
(`native-builtins/src/lang_class.rs::native_class_get_package`, added under
task `T19_H10_GET_PACKAGE` specifically to unblock this exact API pattern
for Hibernate/Logback/JBoss LogManager). It resolves manifest attributes by
finding the class's code-base URL
(`ctx.class_code_base(class_id)`) and, for a **plain `file:` jar** URL,
reading `META-INF/MANIFEST.MF` out of that jar
(`t19_h10_class_manifest_attr` → `plain_jar_manifest_attr`,
`lang_class.rs:13124-13138`). Critically:

```rust
if !path.is_file() {
    return None;
}
```

If `Packager`'s code-base resolves to a **directory** (an exploded Gradle
`build/classes/...` output — the normal shape of a project-dependency
classpath entry in a Gradle multi-project build, as opposed to a packed
`.jar`), this returns `None` unconditionally — no manifest is ever
consulted, so `getImplementationVersion()` returns `null`, and
`Packager.write()` writes a null value for `Spring-Boot-Version`, which
ends up absent from the finished jar's manifest (both `ImagePackagerTests`
and `RepackagerTests` build their test jars via this same `Packager.write`
path and check the result the same way).

**Not confirmed:** whether the real-HotSpot baseline passes because (a) its
Gradle test classpath for `spring-boot-loader-tools` resolves to the built
`.jar` artifact (with a real `Implementation-Version` manifest entry set by
the project's Gradle `jar` task) rather than the exploded classes
directory, and CratonVM's `class_code_base()`/classpath setup for this
suite run instead points at the classes directory even though HotSpot's
identical classpath entry is a jar; or (b) some other difference. This
would need a check of the actual `cratonvm-test-cp.txt` classpath entry for
`spring-boot-loader-tools` used by this run (per
`reference_suite_runner_env_vars_before_isolated_repro` — not available to
inspect from the log files alone) to confirm which shape it is.

**`jarIsOnlyRepackagedOnce()` is a direct downstream consequence, not a
separate bug.** `Packager.isAlreadyPackaged(File)`
(`Packager.java:182-186`) detects "already packaged" purely by checking
whether the file's manifest already has a non-null `Spring-Boot-Version`:

```java
protected final boolean isAlreadyPackaged(File file) {
	try (JarFile jarFile = new JarFile(file)) {
		Manifest manifest = jarFile.getManifest();
		return (manifest != null && manifest.getMainAttributes().getValue(BOOT_VERSION_ATTRIBUTE) != null);
	}
	...
}
```

Since the first `repackager.repackage(NO_LIBRARIES)` call in
`jarIsOnlyRepackagedOnce()` never wrote `Spring-Boot-Version` (per the bug
above), the **second** `repackage()` call's `isAlreadyPackaged()` check
sees no such attribute and concludes the jar is *not yet* packaged —
so it repackages the already-repackaged jar a second time, treating the
first pass's `org.springframework.boot.loader.launch.JarLauncher` (now the
jar's own `Main-Class`) as the "main class" to record as `Start-Class`,
overwriting the real value (`a.b.C`) — exactly the observed
`expected: "a.b.C" but was: "...JarLauncher"`.

## Affected classes

| Module | Class |
|---|---|
| `loader/spring-boot-loader-tools` | `org.springframework.boot.loader.tools.ImagePackagerTests` |
| `loader/spring-boot-loader-tools` | `org.springframework.boot.loader.tools.RepackagerTests` |
