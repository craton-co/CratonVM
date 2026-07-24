# quarkus/runtime: SmallRye/Quarkus Config resolution mismatches — wrong values, missing property names, host-environment leakage

Status: FIXED (4 of 4 original symptoms, including the item-4 residual below). Items 1-3 fixed as of dev commit
`10a561f21` ("fix keycloak quarkus config resolution", 2026-07-13). The item-4 residual (intermittent
`ConfigurationTest::testDatabaseProperties` `ClassCastException`, see "Residual" section below, now retitled
"Residual — CLOSED") stopped reproducing sometime between the 2026-07-13 second investigation pass (dev commit
`10a561f21`) and dev commit `edca766e5` (2026-07-13, same day) — see the 2026-07-13 third investigation pass at the
bottom of the Residual section for full evidence. No source fix was needed/landed in the third pass itself; this
update is a re-verification + doc closure only. A fourth pass later landed a standing (never-fired, unvalidated
against a live repro) class-id canary tripwire in the three suspect stream natives on explicit user request — see
"Update 2026-07-13, fourth pass" at the end of the Residual section.

Date observed: 2026-07-11 (refresh rerun against non-passed-before classes, branch fix/keycloak-nonpassed-rerun-v2-20260710)

**Fixed: 2026-07-13**, commit `10a561f21` on `dev` (author victor-craton, same day as this doc's last "still open"
update — the fix landed within the same 24h window). The fix touched `../../../../native-builtins/src/keystore.rs`,
`../../../../native-builtins/src/phases_late.rs`, `../../../../native-builtins/src/service_loader.rs`, `../../../../native-builtins/src/x509_manager.rs`,
`../../../../native-collections/src/lib.rs`, and `../../../../vm/src/vm/vm_exec.rs`. Key changes:
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

## Residual — CLOSED (no longer reproduces as of dev `edca766e5`): `ConfigurationTest::testDatabaseProperties` intermittent `ClassCastException`

**2026-07-13, third investigation pass — see bottom of this section for the closure evidence.** The
history below (both the original report and the "second investigation pass") is preserved as-is for context; skip to
"Update 2026-07-13, third investigation pass" at the end of this section for the current status.

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
class name inside `native_stream_map`'s per-iteration hot loop in `../../../../native-collections/src/lib.rs`) made the failure
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

**Update 2026-07-13, second investigation pass — ruled out the obvious native-collection GC-pinning sites; still
unresolved**:

1. **`native_hs_stream`/`native_stream_filter`/`native_stream_map`/`native_stream_flat_map` in
   `../../../../native-collections/src/lib.rs` were individually code-reviewed line-by-line and all four already implement the
   correct pin-then-re-read pattern.** `native_hs_stream` in particular has an EXISTING comment explicitly citing
   this exact scenario ("SmallRye's `PropertyMappingInterceptor` hits this through `LinkedHashSet.stream()` while
   enumerating config property names") — it counts first, allocates (which can move the backing map), re-reads the
   backing map through its pin, THEN re-snapshots and writes into the freshly-allocated array. `native_stream_map`
   and `native_stream_filter` pin every element individually via `pin_value_slice`/`read_pinned_elem` before each
   `invoke_virtual` dispatch. `native_stream_flat_map` pins each inner-stream element via `pin_native_root` before
   pushing to the `flat`/`flat_handles` accumulator, and the final `read_value_slice(ctx, &flat_handles, &flat)`
   correctly re-reads every handled slot through its pin. None of these four show an obvious hole.
2. **Widened the diagnostic instrumentation twice** (both reverted, not committed): first checking only `.map()`
   results whose class was exact `java/lang/Object`, then broadening to log EVERY `.map()` call whose *lambda's*
   declaring class name contains `keycloak`/`smallrye`/`picocli`. **Neither ever printed a single match**, including
   in a passing run — meaning either (a) the actual corrupted call flows through a stream/collection native this
   session didn't instrument (there are dozens in this file; `native_stream_sorted`/`native_stream_distinct`/
   `native_al_stream`/`ConcurrentHashMap`-backed variants were not checked), or (b) the corruption happens via a
   completely different mechanism (e.g. a plain `Set`/`Map`/`List` `.add()`/`.put()` storing a stale reference,
   not a stream operation at all — `PropertyMappingInterceptor.iterateNames()`'s `mappersWithoutValues.remove
   (mapper)` calls happen mid-`flatMap`, and the earlier-fixed `cm_lookup_registered`
   (see the FIXED section above) shows this exact class of "stale ref left sitting in a collection, returned later
   without a live-instance check" bug has occurred at least once already elsewhere in this codebase).
3. **The instrumentation-changes-timing effect reproduced with a COMPLETELY DIFFERENT, pre-existing, zero-new-code
   diagnostic**: enabling the existing `CRATONVM_DBG_HEAP_STALE=1` deep heap-walk verifier (`../../../../vm/src/memory/gc.rs`,
   `verify_heap_object_fields`, runs after every GC) also made the failure disappear (1/1 pass, no STALE report) —
   this independently confirms genuine GC-timing sensitivity (not an artifact of the specific instrumentation code
   added), but the heap-walk verifier itself found nothing anomalous in the one run it was tried on, which is
   inconclusive given it only takes one passing run to prove nothing.
4. **Not yet tried**: instrumenting at a lower level than any specific stream op — e.g. a canary check inside
   `pin_native_root`/`read_native_pin` themselves (the shared primitives underneath ALL the above natives) that
   flags any read whose class-id changed unexpectedly between pin and read, with the check kept branch-only (no
   string formatting/eprintln) in the non-anomalous path to minimize the timing perturbation that has masked every
   diagnostic attempt so far. Also not yet tried: bisecting the *minimal* prior-test state needed (currently
   assumed to need all ~72 prior `ConfigurationTest` methods; never confirmed that's actually necessary — a
   standalone repro replaying just the CLI-args/system-property setup without the other 72 tests did NOT
   reproduce, but a partial replay of, say, 10-20 specific prior tests was never tried).

**Update 2026-07-13, third investigation pass — bug no longer reproduces; CLOSING as fixed by dev drift, no source
change from this pass**:

Worktree `C:\data\CratonVM-configtest-race-canary-20260713`, branch `fix/configtest-race-canary-20260713`, branched
from `origin/dev` at commit `edca766e5` ("Merge branch 'fix/picocli-relocate-interceptor-jit-carveout-20260713' into
dev"). `../../../../apps/keycloak` is gitignored and not present in a fresh worktree; ran via
`-KeycloakRoot C:\craton\CratonVM\apps\keycloak` (the main checkout's copy) since building a fresh one was
unnecessary for this investigation — noted as a possible (low-probability, see below) confound since it means test
classes loaded off a different physical directory than in the original investigation.

1. **Prepared, but never needed at the time, the not-yet-tried canary from item 4 above** (a `class_id_of_object`-based
   pin/read mismatch check local to `native_stream_filter`/`native_stream_map`/`native_stream_flat_map` in
   `../../../../native-collections/src/lib.rs` — kept local to each call rather than threaded through the shared
   `native_pin_roots` vector itself, to avoid false positives from the ~100 call sites elsewhere that push/truncate
   that vector directly without going through `pin_native_root`/`unpin_native_roots`). Not merged in this pass — see
   below for why — but landed afterward on user request as a standing tripwire; see "Fourth pass" at the end of this
   section.
2. **Before running the canary, re-confirmed the baseline still fails at the previously-established rate — it did
   not.** A clean release build of unmodified `dev` HEAD (`edca766e5`, binary preserved as
   `target/release/cratonvm-baseline.exe`) ran `ConfigurationTest` alone 8 times back-to-back
   (`-Vm craton -Jit on -TimeoutSec 180 -Parallel 1`, run names `baseline-confirm-run{1..8}-20260713`): **8/8 PASS**,
   0 failures, 0 `ClassCastException` in any `.out`/`.err` log. This already contradicts the ~80%-fail/~20%-pass
   baseline established in the second investigation pass (dev `10a561f21`) enough to warrant checking `git log`
   before assuming the environment was just lucky (see `reference_check_recent_commits_before_fresh_investigation`
   in the shared memory index) — 156 commits landed on `dev` between `10a561f21` and `edca766e5`.
3. **Found two commits directly touching the exact suspect code** (`../../../../native-collections/src/lib.rs` stream/collector
   pinning), landed the SAME DAY as the second investigation pass but evidently after it: `903a38fc1`
   ("pin-stream-collector-before-materialization", fixes a stale-collector-across-GC bug in
   `native_stream_collect`) and `3eb4b6a68` ("fix(streams): pin set elements during collection", fixes an
   unpinned-elements-during-`make_set_of` bug) — both real fixes for unrelated Hibernate/annotation-processing
   `Collectors.toSet()` corruption, not written with this Keycloak bug in mind, but touching the same
   pin/read-through-handle machinery this doc's second pass was auditing.
4. **Isolation test: surgically reverted just those two commits** (`git revert --no-commit 3eb4b6a68 903a38fc1`,
   clean auto-merge, everything else left at `edca766e5` HEAD) and rebuilt (binary preserved as
   `target/release/cratonvm-prefix-revert.exe`). Ran the same 8-rep protocol
   (`prefixrevert-run{1..8}-20260713`): **also 8/8 PASS**, 0 failures, 0 `ClassCastException`. This refutes the
   hypothesis that those two specific commits are what fixed (or incidentally masked) the race — reverting them
   made no observable difference. **16/16 total PASS across both binary variants** — at the previously-established
   ~20% pass rate, 16/16 has probability roughly (0.2)^16 ≈ 6.5e-12 by chance, so this is not sampling noise; the
   race genuinely does not reproduce under this build/environment any more, for a reason other than those two
   commits (most plausibly some other change among the 156 intervening commits — several touch GC/root-scanning
   correctness in this window, e.g. `945e44920` "bracket 5 missing GC-blocking-region locks", `acbea991e`
   "propagate collection overlays from live owners", `ccd51c3a3` "stop class_mirrors from unconditionally rooting" —
   none specifically investigated further since the bug is gone either way).
5. **Sibling classes reverified with the unmodified HEAD binary**: `DatasourcesConfigurationTest` (PASS, 34.5s, 33
   tests), `TracingConfigurationTest` (PASS, 7.9s, 13 tests), `IgnoredArtifactsTest` (PASS, 7.2s, 15 tests) — run
   `sibling-verify-20260713`. No regression.
6. **Caveat / residual uncertainty**: this session ran with `-KeycloakRoot` pointing at a different worktree's
   `../../../../apps/keycloak` copy (cross-directory classpath/jar I/O) rather than a local copy, and per-run wall time was
   noisier than the original investigation's (65s-155s vs. a steady ~84s) — plausibly first-run disk-cache warmup,
   since times settled to 65-95s by run 3 onward with no correlated pass/fail difference. Given the established
   "any added overhead masks this race" pattern from the second pass, a systematically slower environment is the
   one thing that could produce a false "fixed" reading here. However, 16/16 clean passes across two different
   binaries, with per-run times spanning a 2x range and no failures at either extreme, makes "still racy but masked
   by this session's environment" a much weaker explanation than "genuinely no longer reproduces." If it resurfaces,
   the prepared canary (design in this update, not committed — recreate from this description: per-call
   `Vec<Option<ClassId>>` recorded via `class_id_of_object` right after `pin_value_slice`, compared via
   `class_id_of_object` right after each `read_pinned_elem`, atomic-gated single-shot `eprintln!` on mismatch) is
   the next concrete step, along with the never-tried prior-test bisection from item 4 of the second pass.
7. **No source or config change from this pass landed on `dev`** — this update is documentation-only, reflecting a
   fix that arrived incidentally via unrelated commits (or via some other unidentified change) between the second
   and third investigation passes.
8. Evidence: `C:\data\CratonVM-configtest-race-canary-20260713\apps\keycloak-suite-runner\.suite\results\{baseline-confirm-run1..8,prefixrevert-run1..8,sibling-verify}-20260713\all-jit\` (`results.tsv` + `logs/`), same worktree,
   branch `fix/configtest-race-canary-20260713`, HEAD `edca766e5` (unreverted) / isolation-test binary built from
   HEAD with `3eb4b6a68`+`903a38fc1` reverted (not committed, build-only revert, `git revert --abort` afterward to
   restore a clean tree).

**Update 2026-07-13, fourth pass — landed the prepared canary anyway, on explicit user request, as a standing
tripwire (not a fix, and not validated against a live repro since none remains)**:

The user asked for the canary described in item 6 above to be implemented regardless of the bug no longer
reproducing, as a low-cost tripwire in case the race ever resurfaces. Implemented in worktree
`C:\data\CratonVM-configtest-canary-impl-20260713`, branch `fix/configtest-race-canary-impl-20260713`, branched from
`origin/dev` at `ff6d45d6f` (the third-pass closure commit above).

- Added two small helpers, `stream_pin_canary_snapshot`/`stream_pin_canary_check`, and a single
  `STREAM_PIN_CANARY_FIRED: AtomicBool`, directly above `native_stream_filter` in
  `../../../../native-collections/src/lib.rs`. Exactly matches the design sketched in item 6: a `Vec<Option<ClassId>>` snapshot
  taken via `ctx.class_id_of_object` right after each of the three functions' `pin_value_slice` call, compared via
  `ctx.class_id_of_object` again at every `read_pinned_elem` call site for the **input** elements (not the
  freshly-produced output objects in `native_stream_map`/`native_stream_flat_map`, which are pinned individually via
  `pin_native_root` rather than through the snapshot-then-compare pattern, matching the original design's stated
  scope). On a class-id mismatch: a single `eprintln!` gated by an atomic compare-and-swap so it fires at most once
  per process, naming the call site and element index. The non-anomalous path costs one extra `class_id_of_object`
  call (a cheap header read) per element per call site — no allocation, no formatting, no branching beyond the
  comparison itself, in keeping with the original design's goal of not perturbing GC timing enough to mask a future
  recurrence.
- Deliberately did NOT touch the shared `pin_native_root`/`read_native_pin`/`native_pin_roots` machinery in
  `../../../../vm/src/vm/vm_exec.rs` itself, and did NOT extend the canary to the dozens of other native collection/stream
  call sites — scope is exactly the three functions named in the original design, nothing broader.
- **Verification** (the only kind possible here — there is no live repro left to validate detection against):
  clean release build; all three previously-verified sibling classes plus `ConfigurationTest` itself re-run once
  more against the new binary — `DatasourcesConfigurationTest` (33/33 PASS), `TracingConfigurationTest` (13/13
  PASS), `IgnoredArtifactsTest` (15/15 PASS), `ConfigurationTest` (73/73 PASS, 119s) — zero regressions, and the
  canary's `eprintln!` never appeared in any `.err.log` (expected, since the race it watches for is gone). Also ran
  `cargo test --release -p cratonvm-native-collections`: all 6 tests + doctests pass.
- Evidence: `C:\data\CratonVM-configtest-canary-impl-20260713\apps\keycloak-suite-runner\.suite\results\sibling-canary-verify-20260713\all-jit\`.
- If this canary ever fires in a real run, the `eprintln!` output (site name, element index, before/after
  `ClassId`) is the starting point — cross-reference against whatever native stream/collection call preceded it in
  the same test to identify the actual stale-reference source, something every prior pass in this doc failed to
  pinpoint directly.

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
the JIT allow-list (see `../../../../vm/src/jit/skip_list.rs`), unrelated to config-source enumeration or interceptor chains.
Filed as its own new known-issue:
`../../known-issues/keycloak/quarkus-runtime-picocli-arggroupspec-synopsis-hang-20260713.md`. Do not re-attempt to
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
  `quarkus-runtime-picocli-post-compactvalue-hang.md` ("106 tests, failed 28"),
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
