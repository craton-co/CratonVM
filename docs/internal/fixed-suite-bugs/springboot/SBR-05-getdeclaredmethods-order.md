# SBR-05 — `getDeclaredMethods()` returns methods in a different order than HotSpot

**Status:** 🟢 Open — **LOW / won't-fix** (JLS leaves the order unspecified).
**Recommendation:** Leave as-is unless a real test asserts HotSpot's order.

> Decision 2026-06-22: not fixing. `getDeclaredMethods()` order is explicitly
> unspecified, JUnit re-sorts discovered methods itself, and forcing CratonVM's
> reflection enumeration to mirror HotSpot's internal order would be a broad,
> risky change for no spec-required benefit. Documented for completeness only.
**Binary:** `cvsbfull.exe` (dev `df11ac00`) vs HotSpot `jdk-25`.

## Affected probes

`DTags`, `DAnnWalk` (the "first parse* method" lines).

## Symptom

Both probes pick the **first** method whose name starts with `parse` from
`Class.getDeclaredMethods()`:

```java
for (Method mm : c.getDeclaredMethods())
    if (mm.getName().startsWith("parse")) { m = mm; break; }
```

For `org.springframework.boot.build.bom.bomr.version.DependencyVersionTests`:

```
CratonVM: method = parseWhenValidMavenVersionShouldReturnArtifactVersionDependencyVersion
HotSpot:  method = parseWhenVersionWithCombinedPatchAndQualifierShouldReturnCombinedPatchAndQualifierDependencyVersion
```

i.e. `getDeclaredMethods()` enumerates declared methods in a **different order**.

## Root cause (hypothesis)

CratonVM returns declared methods in constant-pool/declaration order (or sorted),
whereas HotSpot returns them in its own internal (effectively reverse/hash)
order. **`java.lang.Class.getDeclaredMethods()` order is explicitly unspecified
by the JLS/Javadoc**, so neither is "wrong" — but the divergence will break any
test that implicitly depends on HotSpot's ordering (JUnit method discovery is
sorted by JUnit itself, so this rarely bites in practice).

## Repro

```bash
cd C:/craton/CratonVM/apps/spring-boot/buildSrc
CP="runner;$(cat test-classpath.txt)"
"C:/craton/CratonVM-sbfull/target/release/cvsbfull.exe" --java-home "C:/Program Files/Java/jdk-25" -cp "$CP" DTags
"C:/Program Files/Java/jdk-25/bin/java.exe" -cp "$CP" DTags
```

## Impact

Mostly cosmetic for spec-compliant code. Listed for completeness; only worth
action if a real Spring Boot test asserts a specific method order.
