# `@ClassPathExclusions` leak — an excluded jar's service registration came back through FOUR process-wide lookups

**Status: FIXED 2026-08-10** on
`fix/springboot-classpath-exclusion-flatscan-20260810` (from `dev@611588fa0`).
Retires both filed pages:

- `jakartaapivalidationexceptionfailureanalyzertests-classpath-exclusion-leak-20260810.md`
- `loggingsystemtests-logbackexcluded-serviceloader-flatscan-20260810.md`

Both were filed the same round from the 139-class non-passed union of the
2026-08-08 `default`/`g1`/`zgc` rerun, each hypothesising one call site and
each explicitly flagging the other as "likely the same bug family — worth
investigating together rather than independently". That instinct was right,
and it under-counted: there were **four** sites, and closing any three of
them still left every test red.

## Verified outcome

`core/spring-boot`, real JDK 25, `run-single-class.ps1`, binaries
`cratonvm-cpexcl-{base,fix-…c}-20260810.exe`:

| Class | before | after |
|---|---|---|
| `…diagnostics.analyzer.JakartaApiValidationExceptionFailureAnalyzerTests` | tests=2 failed=1 | **tests=2 failed=0** |
| `…logging.LoggingSystemTests` | tests=8 failed=1 | **tests=8 failed=0** |
| `…logging.LogbackAndLog4J2ExcludedLoggingSystemTests` | tests=1 failed=1 | **tests=1 failed=0** |

Regression sweep over **all 13** `@ClassPathExclusions` classes in
`core/spring-boot` (the mechanism is generic to the annotation, so the blast
radius is the annotation's users, not the two filed classes): no class that
passed before fails after. Two classes fail identically before and after —
`SpringApplicationNoWebTests` (1/2) and `LoggingApplicationListenerTests`
(33/41) — and are a different defect, see "Not this bug" below.

## The one-line mechanism

A loader built to HIDE a jar answers `getResources` correctly — and four
separate lookups then supplied the answer from somewhere with no notion of
that loader, so the hidden jar's `../../../../apps/META-INF/services` registration (or its
`module-info` equivalent) came back anyway. The provider CLASS stayed
correctly hidden (`ModifiedClassPathClassLoader.loadClass` refuses it), so
`ServiceLoader` read a registration it could not honour and raised

```
java.util.ServiceConfigurationError: <service>: Provider <cn> not found
```

where HotSpot simply discovers no providers. On the Jakarta page the same
shape surfaced as `jakarta.validation.spi.ValidationProvider: Provider
org.hibernate.validator.HibernateValidator not found`, which is why
`ValidationExceptionFailureAnalyzer.analyze` returned null: it recognises
`NoProviderFoundException` and the two "no provider could be found" message
prefixes, and this is neither.

Both filed pages read that message as the JDK `ServiceLoader.fail`
formatting. It is actually CratonVM's own
`service_loader.rs::provider_not_found_error`, raised from the
`loaded == 0 && !missing.is_empty()` arm — deliberately narrowed to "not one
single provider could be built". The distinction matters for anyone
re-triaging a similar message: the absence of `java.util.ServiceLoader.fail`
/ `LazyClassPathLookupIterator` frames in the stack is the tell.

## The four sites

Ordered as they were found; each was confirmed closed by trace before the
next was looked for.

1. **`classloader.rs::ucl_find_resources`** — merged `local_ref.or(std_ref)`,
   so an EMPTY receiver-local scan was replaced by the process-wide flat
   scan. Now the local answer is authoritative whenever the receiver's own
   URL list is knowable (`loader_constructor_url_paths` non-empty); a loader
   CratonVM has no URL view of keeps the historical fallback, because there
   the local scan could not run at all and its emptiness means nothing.
   That distinction is exactly what the Jakarta page asked a fix to draw.
   The SINGULAR `ucl_find_resource` has drawn it since the ModifiedClassPath
   work — this is its plural half, and the two are now spec-consistent.

2. **`classloader.rs::cl_get_resources_impl`, platform-loader receiver** —
   `jdk/internal/loader/*` is a builtin loader class, so a platform receiver
   fell through to the flat scan and handed the whole application classpath
   to every child parented to it. `ModifiedClassPathClassLoader` parents
   itself to platform precisely so its own filtered `URL[]` is the complete
   application view, and its `getResources` is parent-first — so site 1 was
   closed and the jar still arrived, one level up. Restricted to `jrt:`,
   matching what `cl_get_resource` already did for the singular lookup.

3. **`service_loader.rs::discover_providers`, flat classpath scan** — ran
   unconditionally after the loader-scoped `getResources`. Now skipped when
   that loader answered exhaustively. Deliberately **not** gated on the
   loader-scoped pass having FOUND anything: an empty authoritative answer is
   the whole point. (The same guard also covers the per-URL
   `find_all_resource_bytes(&entry_path)` fallback inside that pass.)

4. **`service_loader.rs::discover_providers`, JPMS `module-info provides`** —
   the one that actually kept all three tests red after 1-3 were closed, and
   which neither page anticipated. `service_providers_from_modules` reads ONE
   VM-global module registry with no notion of which loader is asking.
   `logback-classic.jar` and `hibernate-validator.jar` are modular jars that
   DECLARE their provider in `module-info`, so once the flat scan stopped
   offering `../../../../apps/META-INF/services`, the module registry offered the identical
   provider name straight back.

   This one is also a standalone divergence from the JDK, independent of any
   exclusion: **a modular jar reached through the CLASS path is an
   unnamed-module citizen whose `module-info` the JDK ignores outright**, so
   `ServiceLoader` must see only its `../../../../apps/META-INF/services`. Gated on the same
   predicate, which is true exactly for a loader with its own recorded URL
   list — i.e. a class-path loader. The null/builtin-loader case this source
   was added for (`ToolProvider.getSystemJavaCompiler()` needing
   `jdk.compiler`'s `provides javax.tools.JavaCompiler`) is untouched and its
   existing regression still passes.

The shared predicate is
`classloader.rs::loader_owns_complete_resource_view` — deliberately narrower
than the neighbouring `loader_has_recorded_url_set`, which accepts a
`URLClassLoader` whose `ucp` exists but whose URLs were never recorded. That
is precisely the "could not run" case a caller must still fall back for.

## The open question the LoggingSystem page could not resolve

That page flagged, explicitly rather than papering over it, that
`log4j2IsUsedInTheAbsenceOfLogback` PASSES while
`julIsUsedInTheAbsenceOfLogbackAndLog4j2` fails, though both should touch
`SpringFactoriesLoader.<clinit>` → SLF4J on their first line.

Resolved: `logback-classic.jar` is the ONLY jar on this module's test
classpath carrying `../../../../apps/META-INF/services/org.slf4j.spi.SLF4JServiceProvider`
(verified against `cratonvm-test-cp.txt`; there is no `log4j-slf4j2-impl`).
When only logback is excluded, log4j-api is still present, so
commons-logging 1.3.6's `LogFactory.newStandardFactory` selects
`Log4jApiLogFactory` and never reaches `Slf4jLogFactory` — SLF4J's
`ServiceLoader` is never consulted. Only the both-excluded test falls
through to it. Same defect; one test simply never reaches the door.

## Not this bug

The 13-class sweep surfaced two failures that are **not** this leak and were
not changed by the fix (identical counts and messages before and after):

- `LoggingApplicationListenerTests` — 33/41, of which 31 are
  `ClassCastException: org.springframework.core.env.PropertiesPropertySource
  cannot be cast to org.springframework.core.env.PropertySource`.
- `SpringApplicationNoWebTests` — 1/2, `BindException` under
  `logging.group`, plausibly a knock-on of the same.

A class failing to cast to its own supertype is a loader-IDENTITY problem
(one class reached through two loaders), not a resource-visibility one. That
area is already flagged open in
`../spring-boot-core39-residual-clusters-20260723.md` ("a real,
likely-broad classloader-identity bug … blast radius large enough to warrant
its own isolated investigation"). Left there deliberately rather than
attempted alongside a resource-scoping fix.

Also worth recording for
`log4j2-logback-loggingsystemtests-modifiedclasspath-throughput-hang-20260807.md`:
its two classes now complete on this host — `Log4J2LoggingSystemTests` 61/61
and `LogbackLoggingSystemTests` 86/86 — i.e. no hang. That page's own
failure mode was not re-investigated here; this is a data point for whoever
next picks it up, not a verdict on it.

## Tests

Five unit tests, each confirmed to go RED with its own fix reverted (the
falsification was run, not assumed):

- `classloader_tests::test_urlclassloader_find_resources_does_not_fall_back_to_flat_scan`
  — with the process-wide scan primed to hold the descriptor, so the
  assertion cannot pass on an empty fixture.
- `classloader_tests::test_urlclassloader_find_resources_keeps_flat_scan_without_recorded_urls`
  — the "could not run" half stays on the old behaviour.
- `classloader_tests::test_platform_loader_get_resources_excludes_application_classpath`
  — carries an application-loader control in the same test, so an empty
  answer is the platform rule and not an empty classpath.
- `service_loader::tests::discover_providers_skips_flat_scan_for_a_loader_with_its_own_urls`
  — primes BOTH the flat scan and the module registry, so it covers sites 3
  and 4 together.
- `service_loader::tests::discover_providers_keeps_flat_scan_without_a_scoped_loader`
  — control.

`cargo test -p cratonvm-native-builtins --lib`: 3395 passed, 0 failed.

Fixture note for anyone extending these: `MockNativeContext` resolves field
names off an EXACT-class-name table, so a `URLClassLoader` **subclass** needs
its inherited `ucp` declared via `set_declared_fields` or
`set_field_by_name` silently no-ops — and the fixture then presents a loader
with no URLs at all, which is the very state the fix keys off. That cost one
debugging cycle here and would otherwise produce a test that passes for the
wrong reason.

## Reproduce (pre-fix)

```
$env:RUN_SINGLE_CLASS_OUTFILE='out.log'
apps\spring-boot-suite-runner\run-single-class.ps1 `
  -Module 'core/spring-boot' `
  -ClassName 'org.springframework.boot.logging.LogbackAndLog4J2ExcludedLoggingSystemTests' `
  -Exe '<cratonvm>.exe' -ExtraEnv @{CRATONVM_DIAG_SERVICELOADER='1'; CRATONVM_DBG_UCLRES='1'}
```

The two levers answer the whole question in one run, and reading them
together is what separated the four sites:

- `[UCLRES-DBG] … paths=[…] urls=[…]` — the receiver's own URL list and what
  it resolves. `urls=[]` with a non-empty `paths` is the authoritative-empty
  case.
- `[SL-LOADER-DBG] URL: …` — what `loader.getResources` actually handed back.
- `[SL-DBG] … descriptors=N providers=M` — `descriptors` is the flat scan
  alone; **`providers` > 0 with `descriptors=0` and no `[SL-LOADER-DBG] URL`
  line is the module-registry door**, and nothing else looks like that.
