# SBR-13 — `getProtectionDomain()`: classloader is `Object`, permissions are `null`

**Status:** ✅ FIXED (2026-06-23). Both gaps closed; `getProtectionDomain()` now
matches HotSpot's shape (real `AppClassLoader` + non-null empty `Permissions`).
**Recommendation:** FIX — `ProtectionDomain` population gap.

## Root cause (confirmed) + fix

`Class.getProtectionDomain()` on JDK 25 is `getfield protectionDomain; ifnonnull …`
— it does NOT call `getProtectionDomain0()`. CratonVM's registered
`Class.getProtectionDomain` native (`native-builtins/src/lib.rs`) is the live
path. It allocated the `ProtectionDomain` and wrote the empty `Permissions` to
**slot 1** — which is the `classloader` field in the real-JDK
`ProtectionDomain` layout (`codesource`=0, `classloader`=1, `principals`=2,
`permissions`=3), and never set `classloader` at all. So `getClassLoader()`
returned the `Permissions` object (rendered as `Object@HASH`) and
`getPermissions()` was `null`.

Fix:
- New shared helpers in `lang_class.rs`: `build_empty_permissions` (allocs +
  runs the real no-arg `Permissions.<init>` so `toString()`/`elements()` work)
  and `populate_protection_domain_fields` (writes codesource / classloader /
  permissions / principals by **both** slot and name; name-writes run last and
  are authoritative for the real-JDK layout).
- `lib.rs` `getProtectionDomain` override and `native_class_get_protection_domain0`
  both call the helper, resolving `classloader` via
  `native_class_get_class_loader` (bootstrap → null, app → AppClassLoader).

Verified vs HotSpot (`PdProbe`): classloader now
`jdk.internal.loader.ClassLoaders$AppClassLoader@…`, permissions now
`java.security.Permissions@… ( )`. (Residual cosmetic: directory codesource
URL lacks HotSpot's trailing `/` — separate, unrelated to SBR-13.)

## Note (investigated 2026-06-22)

CratonVM builds synthetic `ProtectionDomain`s via
`alloc_default_protection_domain` / `classloader_real.rs` with a placeholder
classloader and no permissions collection. Populating the real owning
`ClassLoader` and an (empty) `java.security.Permissions` is the fix, but it
touches several PD-construction sites and the class-loading path — same
"synthesize JDK object faithfully" theme as SBR-08/09/10/11. Functionally
low-impact (only `getProtectionDomain()` introspection diverges). Medium effort.

**Binary:** `cvsbfull.exe` (dev `df11ac00`) vs HotSpot `jdk-25`.

## Affected probe

`InstProbe` (`org.gradle.api.internal.classpath.DefaultModuleRegistry.getProtectionDomain()`).

## Symptom

`ProtectionDomain.toString()` renders its classloader and permissions:

```
CratonVM (PD render):  ... Object@HASH ...           ... null
HotSpot  (PD render):  ... jdk.internal.loader.ClassLoaders$AppClassLoader@HASH ...
                       ... java.security.Permissions@HASH ( )
```

Two gaps in the `ProtectionDomain` of a loaded class:
1. **classLoader** is a bare `Object` (CratonVM) instead of the actual
   `AppClassLoader`.
2. **permissions** is `null` (CratonVM) instead of an (empty) `java.security.Permissions`.

The functional lines (`getResource`, `CurrentGradleInstallation.get()`,
`gradleHome`) matched HotSpot — only the `ProtectionDomain` contents diverge.

## Root cause (hypothesis)

CratonVM builds a placeholder `ProtectionDomain` for loaded classes without
wiring the owning `ClassLoader` (defaults to a plain `Object`) and without an
empty `Permissions` collection (leaves it `null`). HotSpot always sets both.

## Repro

```bash
cd C:/craton/CratonVM/apps/spring-boot/buildSrc
CP="runner;$(cat test-classpath.txt)"
"C:/craton/CratonVM-sbfull/target/release/cvsbfull.exe" --java-home "C:/Program Files/Java/jdk-25" -cp "$CP" InstProbe
"C:/Program Files/Java/jdk-25/bin/java.exe" -cp "$CP" InstProbe
```

## Impact

Code that reads `class.getProtectionDomain().getClassLoader()` (CodeSource /
security-aware libraries, some classpath scanners) gets a wrong loader and a
`NullPointerException` on `getPermissions()`. Scoped to the `ProtectionDomain`
construction in the class loader.
