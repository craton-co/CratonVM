# `Spring-Boot-Version` manifest attribute missing from packaged jars — fixed

**Status: FIXED 2026-07-18**

## Root cause

The original directory-code-source hypothesis was incomplete. On the exact
Spring Boot runner classpath, `Packager` is loaded from an exploded classes
directory and HotSpot correctly returns `null` from
`getPackage().getImplementationVersion()`. This is valid input: HotSpot's
`Attributes.putValue(name, null)` retains the map entry, and
`Manifest.write()` serializes it as `name: null`.

CratonVM's native `java.util.jar.Attributes` bridge instead treated a null
value as an early-return condition in both `putValue(String, String)` and
`put(Object, Object)`. The mapping was never inserted. Consequently the
packaged manifest had no `Spring-Boot-Version`, and a second Repackager pass
failed to recognize the first output as already packaged, overwriting its
`Start-Class` with `JarLauncher`.

## Fix

Both native insertion paths now pass null values through to the backing map,
while only pinning non-null references. This restores the `Map` contract for
`Attributes` without changing valid non-null insertion behavior.

## Regression coverage and validation

`vm/tests/resources/cratonvm/ManifestNullValue.java` covers:

- direct `LinkedHashMap` null mappings;
- `Attributes.putValue(name, null)` and `Attributes.put(name, null)`;
- lookup and size before serialization; and
- manifest serialization and reparse.

The source-matched Spring Boot 4.1.0-SNAPSHOT fixture and the standalone
regression were run with Eclipse Adoptium JDK 25.0.3.9 and the uniquely named
`cratonvm-loader-tools-version-manifest-20260718.exe` binary:

| Probe | JIT | `--nojit` |
|---|:---:|:---:|
| `ManifestNullValue` | PASS | PASS |
| `ImagePackagerTests` (37 tests) | PASS | PASS |
| `RepackagerTests.springBootVersion()` and `jarIsOnlyRepackagedOnce()` | PASS | PASS |

The full `RepackagerTests` class still has the independently tracked
zip-fidelity failures in
`docs/known-issues/springboot/loader-tools-manifest-entries-and-zip-fidelity-residuals.md`:
six CRC/layout failures plus `signedJar()` under JIT, and `signedJar()` under
`--nojit`. Neither the fixed version attribute assertion nor the downstream
double-repackage assertion is among them.
