# `java.util.Locale` real-JDK bootstrap: `sun.util.locale.BaseLocale` permanently fails to initialize, breaking any un-overridden `Locale` method (`toString`, `equals`, `hashCode`, …)

Status: open — root cause identified with a precise, reproducible repro chain; fix not yet implemented (requires reordering VM bootstrap, out of scope for the session that found it).

Date observed: 2026-07-14, while trying to verify
`keycloak/unsafe-memoryaccessoption-repair-field-index-false-positive.md`
against a real Keycloak `testsuite/model` run — this bug independently blocks that
verification (every `testsuite/model` class needs the `Jpa` model parameter, whose
Liquibase provider factory init touches `Locale`), so it is a real, unrelated blocker
worth tracking on its own.

## Summary

Under real-JDK mode (`--java-home <realjdk>`), any `java.util.Locale` method that is
**not** one of CratonVM's native overrides (`toString`, `equals`, `hashCode`, …) throws
`NullPointerException: Cannot invoke "sun.util.locale.BaseLocale.getLanguage()" because
"this.baseLocale" is null` — even on the object returned by `Locale.getDefault()`,
which CratonVM natively constructs and is *supposed* to have `baseLocale` populated.

Minimal repro (no Keycloak needed):
```java
import java.util.Locale;
public class P {
    public static void main(String[] a) {
        System.out.println(Locale.getDefault());  // prints "java.util.Locale@<hex>" (Object.toString(), not Locale's)
        System.out.println(Locale.US);             // throws NoClassDefFoundError: java/util/Locale
    }
}
```
Real `java` (same JDK) prints `en` then `en_US` — this is CratonVM-only.

## Root cause chain

1. `../../../native-builtins/src/locale_bootstrap.rs`'s `register()` installs a native override
   for `java/util/Locale.getDefault()` (`get_or_create_default()`), because real
   `Locale.<clinit>` walks the `LocaleProviderAdapter` ServiceLoader chain and throws
   `InternalError("should not come down here")` in CratonVM's partial bootstrap.
2. `get_or_create_default()` allocates a real-class-stamped synthetic `Locale` object
   (`alloc_concurrent_synthetic(ctx, "java/util/Locale", 32)`) and calls
   `../../../native-builtins/src/lib.rs`'s `locale_populate()` to fill it in — including a real
   `sun.util.locale.BaseLocale` object assigned to the `baseLocale` instance field, so
   that un-overridden real-bytecode `Locale` methods (which all read `this.baseLocale`)
   don't NPE.
3. `locale_populate()` builds that `BaseLocale` via
   `ctx.ensure_class_initialized("sun/util/locale/BaseLocale")`. **This call fails.**
   Confirmed via targeted debug tracing (this session): it returns
   `Err(InternalError(Linkage(NoClassDefFoundError { class_name: "sun/util/locale/BaseLocale" })))`.
   Per JVMS §5.5, `NoClassDefFoundError` on a *repeated* init attempt means the class
   was already marked `InitializationError` by an **earlier, different** touch —
   this is a cached failure, not today's failure.
4. Isolated repro of that earlier failure (`Class.forName("sun.util.locale.BaseLocale")`
   run standalone, no prior Locale touch) surfaces the *original* exception:
   **`java.lang.InternalError: null property: java.home`**. This is real JDK
   bytecode's own error text — it comes from `jdk.internal.misc.VM.getSavedProperty`,
   which throws exactly this when the key isn't in the VM's saved-properties snapshot.
   (Confirmed this is NOT a CratonVM-authored string — `grep -rn "null property"` over
   `../../../vm/src` and `../../../native-builtins/src` finds no match; it's JDK-internal bytecode text.)
5. CratonVM natively overrides `VM.getSavedProperty` too
   (`native-builtins/src/lang_system.rs::native_vm_get_saved_property`), delegating to
   `ctx.get_system_property("java.home")`. **That returns `None`** at the moment
   `BaseLocale`'s `<clinit>` (transitively, via `LocaleProviderAdapter`-adjacent code)
   first asks for it — even though `java.home` **is** correctly set by the time user
   code runs (confirmed: a probe printing `System.getProperty("java.home")` from
   `main()` correctly shows the `--java-home` value).

**Net conclusion: this is a boot-ordering bug, not a missing feature.** Something
touches `sun/util/locale/BaseLocale` (indirectly, via the `Locale.getDefault()` native
override — itself triggered very early, since `Locale.<clinit>` is documented in
`locale_bootstrap.rs`'s own top-of-file comment to call `getDefault()`) **before**
`shared.system_properties` (populated in `../../../vm/src/vm/vm_init.rs`'s giant `SharedVm::new()`,
around the "Tier 3: env/config-derived keys" comment near line 2450) has `java.home` in
it. Once `BaseLocale` fails once, JVMS §5.5 makes that failure permanent for the rest of
the process — no retry, no later successful attempt, regardless of how quickly
`java.home` actually becomes available afterward.

## Why this wasn't caught by existing tests

- CratonVM's own unit tests for Locale (`../../../vm/src/vm.rs` `locale_default_and_getters` /
  `p93_locale_default_and_to_string`) construct a fresh, minimal `SharedVm` directly in
  the test — a very different code path/timing than the full CLI boot sequence
  (`vm-cli` → `SharedVm::new()` → class loading → main thread → user bytecode), so they
  don't reproduce the ordering race.
- `getLanguage()`/`getCountry()`/`toLanguageTag()` are separately natively overridden
  (reading from a Rust-side `synthetic_locale_data` side table, not `baseLocale`), so
  they work fine regardless of this bug — masking the problem for any code that only
  calls those. `toString()`, `equals()`, `hashCode()`, and anything else that runs real
  bytecode touching `baseLocale` directly are what actually break.

## Next steps

1. Find exactly what triggers the first `Locale.getDefault()` (and hence
   `BaseLocale.<clinit>`) touch during boot, and confirm it happens before
   `shared.system_properties` is populated with `java.home`. The population code is in
   `../../../vm/src/vm/vm_init.rs`'s `SharedVm::new()` (huge function, ~850–2627), in a block
   starting around line 2252 (`let mut sys_props = HashMap::new();`) with `java.home`
   itself set around line 2456, under the "Tier 3: env/config-derived keys" comment.
   Determine whether the early touch happens *within* `new()` (some earlier bootstrap
   step in that same function reaching Locale) or *after* `new()` returns (some
   post-construction boot phase running before the properties are flushed anywhere
   `get_system_property` can see them).
2. Fix by ensuring `java.home` (and ideally the full property set from that Tier-1/2/3
   block) is available to `ctx.get_system_property` **before** anything can trigger
   `Locale`/`BaseLocale` initialization — e.g. hoist just the `java.home` computation
   (it only depends on `config: &VmConfig`, already available from the very start of
   `new()` — see `resolve_java_home_public(config.java_home.as_deref())`) to the top of
   `SharedVm::new()`, ahead of any class-loading bootstrap step. Verify the hoist is
   safe (nothing earlier in `new()` depends on `sys_props` NOT yet containing
   `java.home`) before landing it — this touches a very large, sensitive bootstrap
   function, so change it surgically and re-run at least the existing Locale/System-
   properties unit tests (`shared_vm_system_properties_populated`,
   `shared_vm_custom_system_properties`, `locale_default_and_getters`,
   `p93_locale_default_and_to_string`) plus a real end-to-end `--java-home` run.
3. Re-verify with the minimal repro above, then with the full real-Keycloak
   `testsuite/model` repro (see the harness notes below) — this bug was independently
   blocking that verification for the Unsafe/MEMORY_ACCESS_OPTION doc.
4. A very low-noise diagnostic (`tracing::warn!` on the `BaseLocale` init failure) was
   added in `../../../native-builtins/src/lib.rs`'s `locale_populate()` as part of this session's
   investigation (2026-07-14) — behavior-neutral, purely observability, safe to keep.

## Repro / harness notes (for whoever picks this up)

- Minimal probe: any `java.util.Locale` method call after `Locale.getDefault()` that
  isn't `getLanguage`/`getCountry`/`toLanguageTag` reproduces it — `toString()` is the
  simplest (`System.out.println(Locale.getDefault())`).
- A full real Keycloak (26.6.1, `github.com/keycloak/keycloak.git`) checkout with
  `testsuite/model` fully built (main + test classes compiled) is available on the
  Azure build host (`victor@20.83.144.174`) at
  `/data/data/wt-keycloak-memaccess-fieldindex-20260714/apps/keycloak` — build with
  `JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64` (the host's default JDK 21 is
  JRE-only, no `javac`) and
  `-Dmaven.repo.local=/data/data/m2-keycloak-memaccess-fieldindex-20260714/repository`
  (the default `/home/victor/.m2` doesn't have this checkout's 26.6.1 artifacts, and
  `/` fills up fast on this host — always redirect `TMPDIR`/`maven.repo.local`/
  `CARGO_TARGET_DIR` off it). A hand-written `../../../apps/keycloak/kc-runner/KcRunner.java`
  (JUnit Platform Launcher wrapper, prints
  `KCRUNNER_RESULT tests=N failed=N aborted=N skipped=N containersFailed=N` /
  `KCRUNNER_LOAD_FAIL`) is compiled there too — reusable harness; see
  `../../../apps/keycloak-suite-runner/run-keycloak-suite.ps1` (pwsh works on this host) for the
  invocation contract, including the `-Dkeycloak.model.parameters=Infinispan,Jpa` etc.
  system properties `testsuite/model` needs.
- This bug reproduces via `org.keycloak.testsuite.model.authz.ConcurrentAuthzTest`'s
  boot path: `DefaultKeycloakSessionFactory.loadFactories` →
  `DefaultLiquibaseConnectionProvider.init()` → `liquibase.database.Database.<clinit>`
  → touches `Locale` → `NoClassDefFoundError: java/util/Locale`. Since the `Jpa` model
  parameter is required transitively by Keycloak's storage wiring (removing it just
  produces a different `RuntimeException: No provider factories exists for provider
  JpaConnectionProvider`), this is unavoidable for any `testsuite/model` class on this
  checkout — not a Liquibase-specific quirk, a general real-JDK Locale bug.

---

## FIXED 2026-07-14

Fixed as a side effect of commit `f62d2073` ("pin `java.util.Properties`
side-table bridges to `NativeKind::Bridge`") — the exact chain this doc's
root-cause section traced (`Locale.<clinit>` → `BaseLocale.<clinit>` →
`StaticProperty.<clinit>` → `StaticProperty.getProperty("java.home")`) bottoms
out in `System.getProperties()`'s side-table read, which is what that commit
restored. Not a boot-ordering hoist after all (the "reorder `SharedVm::new()`"
next-step above turned out not to be necessary) — the actual defect was the
same silently-dropped-`SyntheticStub` pattern as the `String.getBytes()`
bug (see
[`string-getbytes-empty-real-jdk-mode-FIXED.md`](string-getbytes-empty-real-jdk-mode-FIXED.md),
now also `..`), just reached via a different call chain.

Verified: `Locale.getDefault()` and `Locale.US` both correctly print `en_US`
(previously: `Object.toString()` fallback / `NoClassDefFoundError`).
