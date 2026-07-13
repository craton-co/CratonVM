# quarkus/runtime: SmallRye/Quarkus Config resolution mismatches — wrong values, missing property names, host-environment leakage

Status: FIXED (3 of 4 original symptoms) as of dev commit `10a561f21` ("fix keycloak quarkus config resolution",
2026-07-13). One narrower residual remains OPEN — see "Residual" section below — tracked inline here rather than
in a separate known-issues doc because it is a direct descendant of item 4 below, not a new symptom.

Date observed: 2026-07-11 (refresh rerun against non-passed-before classes, branch fix/keycloak-nonpassed-rerun-v2-20260710)

**Fixed: 2026-07-13**, commit `10a561f21` on `dev` (author victor-craton, same day as this doc's last "still open"
update — the fix landed within the same 24h window). The fix touched `native-builtins/src/keystore.rs`,
`native-builtins/src/phases_late.rs`, `native-builtins/src/service_loader.rs`, `native-builtins/src/x509_manager.rs`,
`native-collections/src/lib.rs`, and `vm/src/vm/vm_exec.rs`. Key changes:
- `ConfigSourceContextConfigSource.getPropertyNames()` gained a native override that filters a context iterator
  down to genuine `String` entries only, instead of leaking raw/non-String placeholder objects.
- `cm_lookup_registered` (SmallRye config-mapping cache lookup) now validates the cached candidate via
  `Class.isInstance` before returning it, instead of trusting any non-null value in the mappings table — this is
  what fixed the `SmallRyeConfigSourceInterceptorContext.proceed` `NoSuchMethodError` on a plain `java/lang/Object`
  receiver described in the 2026-07-13 update below.
- `native_smallrye_get_config_mapping` gained a dedicated path for `io.smallrye.config.source.keystore.KeyStoreConfig`
  that registers the mapping through SmallRye's own `ConfigMappings.registerConfigMappings` API before falling back
  to the lower-level construction path.
- Several hand-stubbed native overrides were REMOVED so real Keycloak bytecode now runs instead:
  `PropertyMappingInterceptor.hasInferredValue`, `LoggingPropertyMappers.isMdcActive`,
  `TracingPropertyMappers.isTracingEnabled`, `TracingPropertyMappers.isTracingAndEmbeddedInfinispanEnabled`. These
  were producing hardcoded/wrong boolean values (root cause of item 2 below).
- `java.io.File`'s absolute-path natives now strip a trailing separator to match real `File.getAbsolutePath()`
  normalization (affects `kc.home.dir`-derived paths used by keystore/DB config resolution).

## Verification (2026-07-13, this session)

Re-verified against a fresh local build of current `dev` (worktree
`C:\data\CratonVM-quarkusconfig-verify-20260713`, branch `verify/quarkus-config-resolution-20260713`; built and
tested on the local Windows box — the usual Azure build host, `victor@20.83.144.174`, had a 1+ hour SSH outage this
session, consistent with [[feedback_azure_host_extended_outage_20260713]], so this investigation ran locally
throughout with the user's explicit sign-off):

```
apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 180 -Parallel 1 -ClassList <5 classes below> -Exe <local build> -JdkHome "C:\Program Files\Java\jdk-25"
```

Results (`.suite\results\verify-quarkus-config-20260713\all-jit\`):
- `DatasourcesConfigurationTest` — **PASS** (99.6s; covers item 1 `propagatedPropertyNames` AND the 2026-07-13-update
  `testMysqlTLSOptions` `NoSuchMethodError` — both confirmed fixed)
- `TracingConfigurationTest` — **PASS** (15.2s; covers item 2 `syslogLogMdcOn`)
- `IgnoredArtifactsTest` — **PASS** (13.4s; covers item 3)
- `ConfigurationTest` — **FAIL** intermittently (72/73 tests pass; `testDatabaseProperties` fails most runs) — see
  Residual below. This is a DIFFERENT failure mode than item 4's original `expected:<secret> but was:<null>`
  (which no longer reproduces — that specific symptom is fixed).

Items 1, 2, and 3 are confirmed FIXED. Item 4 is narrowed and downgraded from "value doesn't resolve" to a
narrower, intermittent residual described below.

## Residual (OPEN): `ConfigurationTest::testDatabaseProperties` intermittent `ClassCastException`

**Symptom**: `java.lang.ClassCastException: java.lang.Object cannot be cast to java.lang.String` thrown from
`io.smallrye.config.SmallRyeConfig$ConfigSources$PropertyNames.latest(SmallRyeConfig.java:1163)` — a `checkcast
String` on the value returned by `Iterator.next()` while draining `SmallRyeConfigSourceInterceptorContext
.iterateNames()`. Full chain: `Configuration.getPropertyNames()` → `SmallRyeConfig.getPropertyNames()` →
`PropertyNames.get()` → `PropertyNames.latest()`, reached via
`Environment.getCurrentOrCreateFeatureProfile()` → `QuarkusSingleProfileConfigResolver.<init>` →
`AbstractConfigurationTest.createConfig()`, called from `createConfigFromCliArguments("--db=dev-file")` at
`ConfigurationTest.java:381`.

**Confirmed genuinely intermittent, not deterministic**: 5 back-to-back reruns of `ConfigurationTest` alone (same
binary, same classpath, same `-TimeoutSec 180 -Parallel 1`) FAILED 4/5 times and PASSED 1/5 (~80% fail rate). A
standalone Java reproduction of the exact same steps (`KeycloakMain.reset`, set the 3 `System.setProperty` calls,
`ConfigArgsConfigSource.setCliArgs("--db=dev-file")`, `Configuration.resetConfig()`,
`Environment.getCurrentOrCreateFeatureProfile()`, `Configuration.getConfig()`, `PropertyMappers.reset()`,
`PropertyMappers.sanitizeDisabledMappers(new Start())` — see the removed scratch probe
`org.keycloak.quarkus.runtime.configuration.DbRepro`) did **not** reproduce in isolation — the bug needs the
accumulated static state left behind by the other 72 tests in the class (order-dependent), not just a clean
`--db=dev-file` config build.

**Strong evidence this is a genuine race/GC-timing bug, not a fixed logic error**: adding a diagnostic
`eprintln!`-based instrumentation (gated behind a new `CRATONVM_DBG_STREAM_MAP` env var, reading each element's
class name inside `native_stream_map`'s per-iteration hot loop in `native-collections/src/lib.rs`) made the failure
disappear completely — 6/6 runs PASSED with the instrumentation enabled vs 5/5 FAILED without it, using the
otherwise-identical binary (the instrumentation was purely diagnostic — read-only `class_name_of_id` lookups plus a
conditional `eprintln!`, no behavior change on the success path). The instrumentation itself never printed a match
(its filter narrowed to lambda classes naming `keycloak`/`smallrye`/`picocli`, which never fired), meaning the
*specific* corrupted call site was not pinpointed — but the mere extra per-iteration native-call overhead was
enough to shift GC/scheduling timing away from whatever race produces the corrupted `Object`-typed element. This
matches the broader, well-established pattern in this codebase of "stale collection reference across a moving
young-gen GC" bugs (see [[reference_moving_young_gen_complete_coverage]],
[[reference_stale_ref_decode_hardening]]) — some native collection/stream call in the
`PropertyMappingInterceptor.iterateNames()` → `mappersWithoutValues.stream().filter(m -> hasInferredValue(m,
context)).map(m -> m.getTo())` → `IteratorUtils.chainedIterator(...)` path (the leading suspect, since
`hasInferredValue` recursively re-enters the interceptor chain via `context.restart(key)` mid-iteration, and this
whole chain is exactly what feeds `SmallRyeConfigSourceInterceptorContext.iterateNames()`) most likely returns a
stale/pre-GC-move `Object` reference instead of a re-pinned `String` under specific timing — but the exact
allocation/pin gap was not isolated within this session's time budget.

**Next steps for whoever picks this up**:
1. Re-add the `CRATONVM_DBG_STREAM_MAP`-style instrumentation but widen the class-name filter (or drop it
   entirely and log every `.map()`/`.filter()` call whose *input* element is exact class `java/lang/Object`) to
   catch the actual corrupted call, since the narrower `keycloak`/`smallrye`/`picocli` substring filter used this
   session never matched (the lambda's `class_name_of_id` may return a JVM-internal hidden-class name without a
   readable package prefix).
2. Audit `native-collections/src/lib.rs`'s `native_hs_stream`/`native_stream_filter`/`native_stream_map` GC-pinning
   for a case where a `Set<T>.stream()` snapshot element is read AFTER a nested nativeーtoJava callback
   (`ctx.invoke_virtual`) inside the SAME iteration allocates and moves the young generation, without a
   pin/re-read cycle — `PropertyMappingInterceptor.hasInferredValue`'s `context.restart(key)` reentrant call is the
   prime candidate for such a nested allocation trigger.
3. Since it needs 72 prior tests' accumulated state to reproduce, isolate the *minimal* prior state by bisecting
   which specific earlier `ConfigurationTest` method(s) are required before `testDatabaseProperties` for the race
   to manifest, rather than assuming the full class is needed.

## 2026-07-13 update correction: the PicocliTest hang is a SEPARATE, unrelated bug — do NOT treat as shared root cause

The 2026-07-13 update below (now historical) speculated that `PicocliTest`'s 27/107 failures "may share a root
cause" with this doc's config-resolution findings, since both exercise SmallRye Config. This session investigated
`PicocliTest` and that hypothesis is **refuted**: `PicocliTest` genuinely HANGS (confirmed via CPU-time flatlining
across a 20s window, and via CratonVM's own `--stack-dump-on-timeout` watchdog thread dump), and the dump shows the
single `main` thread stuck 40+ frames deep inside **picocli's own CLI help-text rendering**
(`Picocli.addCommandOptions` → `addMappedOptionsToArgGroups` → `CommandLine$Model$ArgGroupSpec.Builder.build()` →
`synopsisUnit()` → `rawSynopsisUnitText()` → `concatOptionText()` → `Help$ColorScheme.optionText()` →
`Help$Ansi$Text.<init>()`) — building the ANSI-styled command-line synopsis text for the `start-dev` command's
option groups, entered during `PicocliTest.otelLogsHeaders()`'s very first `pseudoLaunch(...)` call, BEFORE any
SmallRye config-source/interceptor code is reached at all. This is purely interpreter-mode picocli/ArgGroupSpec
text-building performance (or a genuine infinite loop within it) — `picocli/` packages are deliberately kept off
the JIT allow-list (see `vm/src/jit/skip_list.rs`), unrelated to config-source enumeration or interceptor chains.
Filed as its own new known-issue:
`docs/known-issues/keycloak/quarkus-runtime-picocli-arggroupspec-synopsis-hang-20260713.md`. Do not re-attempt to
fix the PicocliTest hang as part of this doc's scope.

**Update 2026-07-13 (historical, superseded by the fix above)**: confirmed still genuinely open via a fresh (non-stale-distribution) HotSpot comparison —
HotSpot cleanly passes both of the following, CratonVM still fails them:
- `quarkus/runtime :: DatasourcesConfigurationTest::testMysqlTLSOptions` now additionally shows
  `java.lang.NoSuchMethodError: java/lang/Object.proceed(Ljava/lang/String;)Lio/smallrye/config/ConfigValue;`
  thrown from `SmallRyeConfig$SmallRyeConfigSourceInterceptorContext.proceed` — the receiver is reported as
  plain `java/lang/Object` rather than the real interceptor-context type, suggesting a wrong/erased receiver
  type on a dynamically-chained interceptor object, distinct from the value-mismatch symptoms already described
  above.
- `quarkus/runtime :: PicocliTest` still fails 27/107 methods (`errorSpiBuildtimeChanged`,
  `buildOptionChangedWithOptimized`, `spiAmbiguousSpiAutoBuild`, and 24 others) with plain JUnit `AssertionError`s
  from `PicocliTest.build()` — same rough failure count as previously observed in
  `docs/internal/fixed-suite-bugs/quarkus-runtime-picocli-post-compactvalue-hang.md` ("106 tests, failed 28"),
  which explicitly deferred root-causing these as "later behavioral assertions ... not root-caused here". Given
  both classes exercise SmallRye Config resolution/interceptor chains, these may share a root cause with this
  doc's config-resolution-mismatch findings.
- Evidence: `apps/keycloak-suite-runner/.suite/results/nonpassed-before-refresh2-shard1/all-jit/logs/quarkus_runtime.org.keycloak.quarkus.runtime.configuration.DatasourcesConfigurationTest.{out,err}.log` and
  `.../quarkus_runtime.org.keycloak.quarkus.runtime.cli.PicocliTest.{out,err}.log`; fresh HotSpot PASS in
  `apps/keycloak-suite-runner/.suite/results/hotspot-refresh-v2-shard1/hotspot-jit/results.tsv` (both classes in
  the 162-class `timeout-affected.tsv` sample, branch `fix/keycloak-nonpassed-rerun-v2-20260710`).

## Summary (original symptoms, historical)

Several `quarkus/runtime :: configuration.*` test classes fail with config-value or config-property-enumeration
mismatches:

1. **`DatasourcesConfigurationTest::propagatedPropertyNames`** — expects the enumerated set of config property
   names to contain `"quarkus.datasource.jdbc.min-size"`, but the actual enumerated set instead contains a huge
   dump of unrelated **host machine environment variables and system properties** (`PATH`,
   `CLAUDE_CODE_SESSION_ID`, `ANTHROPIC_BASE_URL`, `CUDA_PATH_V12_6`, `chocolateylastpathupdate`, hundreds more) —
   none of which are Quarkus/Keycloak config values, and the specific expected Quarkus-generated property name is
   simply absent from the (very large) actual set. **FIXED.**

2. **`TracingConfigurationTest::syslogLogMdcOn`** (and presumably others in the same class) — expects
   `quarkus.otel.enabled` to resolve to `"true"` but gets `"false"`. **FIXED** (root cause: hardcoded native stub
   `TracingPropertyMappers.isTracingEnabled` returning a fixed wrong value; stub removed, real bytecode now runs).

3. **`IgnoredArtifactsTest`** — `AssertionError: Ignored artifacts does not comply with the specified artifacts for 'dev-file' JDBC driver`. **FIXED.**

4. **`ConfigurationTest`** — `AssertionError: expected:<secret> but was:<null>` (a keystore-backed config value
   not resolving). **FIXED** (root cause: `KeyStoreConfig` mapping wasn't registered through SmallRye's own API;
   see the keystore.rs changes above) — **but see the Residual section above for a narrower, intermittent
   follow-on failure in the same test class.**

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-quarkus-config -ClassList <(printf 'module\tclass\nquarkus/runtime\torg.keycloak.quarkus.runtime.configuration.DatasourcesConfigurationTest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v2-refresh-20260711.exe -JdkHome $jdk
```

## Evidence

Original: `C:\craton\CratonVM-keycloak-nonpassed-v2-20260710\apps\keycloak-suite-runner\.suite\results\nonpassed-before-refresh-shard1\all-jit\logs\quarkus_runtime.org.keycloak.quarkus.runtime.configuration.{DatasourcesConfigurationTest,TracingConfigurationTest,IgnoredArtifactsTest,ConfigurationTest}.out.log`,
2026-07-11 refresh rerun with a binary built from current `dev`.

2026-07-13 verification: `C:\craton\CratonVM\apps\keycloak-suite-runner\.suite\results\verify-quarkus-config-20260713\all-jit\` and
`verify-configtest-rerun{1,2,3,4}-20260713\all-jit\` (fail reruns) /
`verify-configtest-dbg2-run{1,2,3,4,5}-20260713\all-jit\` (pass reruns, with diagnostic instrumentation), local
Windows-box build from worktree `C:\data\CratonVM-quarkusconfig-verify-20260713` at `dev` commit `10a561f21`.
