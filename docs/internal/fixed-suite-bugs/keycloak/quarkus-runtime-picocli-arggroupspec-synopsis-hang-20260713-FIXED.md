# quarkus/runtime PicocliTest hang — exponential-ish RelocateConfigSourceInterceptor fan-out under the JIT skip-list

Status: **RESOLVED (2026-07-14)** — the full `PicocliTest` class (107 tests) now runs to completion with no hang,
verified twice against independently-built binaries (see "Resolution" below). The doc's own leading theory for why
a fix would be hard — an O(2^N) exponential interceptor fan-out with N possibly in the 30-40 range — is **refuted
by direct measurement: the real N is 3**. No further JIT/native change was made or is believed necessary; this
looks like the cumulative effect of the many other interpreter/dispatch-throughput fixes that landed in `dev`
between 2026-07-13 and 2026-07-14, not one identifiable single fix. See "Resolution (2026-07-14)" for the full
verification writeup. The original 2026-07-13 investigation (root cause, the KC26-PIC.2 carve-out, the "do not
naively lift the skip-list" caveat) is preserved below unchanged as historical record — it remains accurate as a
description of what was true on 2026-07-13 and why the carve-out is still a reasonable thing to keep.

## Resolution (2026-07-14)

**Full-class run, no hang:** ran the complete 107-test class (via a custom timing-instrumented JUnit Platform
runner, `TimedKcRunner`, against the module's real ~46KB Maven test classpath on a Linux build host) twice:

- Once against a binary at `b30cbbb7` (57 commits behind the then-current `dev` tip, used as a stand-in because
  tip had a transient, unrelated regression — see below): **106/106 tests started and finished** (1 `@Ignore`d),
  **80 passed / 26 failed (assertion errors, not hangs/timeouts) / 0 aborted**, total wall time **199.8s**.
- Once against the TRUE current `dev` tip at the time (`f62d2073`), after that unrelated regression was fixed:
  **identical tally** — 107 found, 1 skipped, 106 started, 106 finished, 80 passed, 26 failed, total wall time
  **213.5s** (`RUN-END elapsedMs=213478`). `--stack-dump-on-timeout 300` was armed for both runs; it never fired —
  there was nothing to dump.
- Both runs used the DEFAULT skip-list configuration (KC26-PIC.2's carve-out active, nothing manually widened via
  `CRATONVM_JIT_ALLOW_PACKAGES`) — i.e. this is what a normal invocation gets today, not a special-cased "everything
  unbanned" run. That is a better outcome than what this doc originally asked for: the safe default now suffices.

**The 26 assertion failures are a separate, distinct issue, NOT part of this doc's scope.** They're real Java-level
`AssertionError`s (e.g. `errorSpiBuildtimeChanged`, `warnProviderChanged`, `optimizedExport`,
`testNoReaugFromDevToDevExport`), not hangs or timeouts, and were never visible before because the class never
previously ran far enough to reach most of these tests. A separate session has been spun up to triage whether
they're real CratonVM correctness bugs, test-harness/environment gaps, or pre-existing Keycloak-side flakiness —
see whatever `../../../known-issues/keycloak` doc that produces once it lands (not yet filed as of this writing).

**N measurement (refutes the exponential-fan-out theory):** patched a real copy of `RelocateConfigSourceInterceptor`
(pulled from `smallrye-config` GitHub tag `3.16.0`, matching Keycloak 26.6.1's actual dependency) with
instance/call counters, placed ahead of the real jar on the classpath, and ran the full class. Result: **N=3
stacked instances per config build** (2 `Function`-mapping instances from
`QuarkusConfigBuilderCustomizer$1$1`/`$2$1`, plus 1 `Map` instance for `smallrye.config.profile`→`mp.config.profile`)
— not the 30-40 the "Next steps" section below speculated might be needed to explain minutes-long stalls. 2³ = 8
total interceptor invocations per property lookup is not, on its own, an exponential-blowup-scale cost; whatever
made the class take 5+ minutes and eventually stall on 2026-07-13 was **rebuild frequency (247 SmallRyeConfig
rebuilds observed across the run) times per-call interpreter dispatch overhead, not chain depth**. This means the
"Next steps" item below about memoizing `RelocateConfigSourceInterceptor.getValue()`/adding a native fast-path for
it was based on a since-refuted premise — implementing it now would be solving a problem that measurably doesn't
exist at this scale, so it was deliberately NOT done. (CratonVM does have a general mechanism for this exact kind of
targeted native override — `NativeMethodRegistry`, `../../../../native-api/src/registry.rs` — including precedent for
overriding ordinary, non-`native`-declared methods on third-party/non-JDK library classes, e.g. Quarkus's
`LocaleConverter.convert`. If a future regression reintroduces this class of slowdown, re-measure N first — don't
assume the old 30-40 speculation was ever right, since it wasn't.)

**Unrelated regression hit during verification, not part of this bug:** the initial attempt to verify against true
`dev` tip (`6addc1e0`) hit `InternalError: null property: java.home` (`Locale.<clinit>` →
`StaticProperty.getProperty("java.home")`), a same-day regression from `d8092acb` ("fix-tests-real-jdk-contracts")
that broke real-JDK mode broadly by dropping `java.util.Properties`' side-table native bridges under
`set_drop_synthetic_stubs(true)`. Already found, root-caused, and fixed by a concurrent session one commit later
(`f62d2073`, "pin java.util.Properties side-table bridges to `NativeKind::Bridge`") — not something this
investigation needed to fix, just something it had to route around (via a throwaway pre-regression binary) and
then re-verify against once the fix landed.

---

## Original 2026-07-13 investigation (preserved verbatim as historical record)

Date observed: 2026-07-13, split out from
`keycloak-quarkus-runtime-config-resolution-mismatches.md`, whose 2026-07-13 update
speculatively grouped this with SmallRye Config resolution mismatches ("may share a root cause"). That hypothesis
is refuted below — this is a separate, unrelated bug.

## Summary (updated 2026-07-13, second investigation pass)

`quarkus/runtime :: org.keycloak.quarkus.runtime.cli.PicocliTest` genuinely stalls for a very long time (confirmed
NOT a classic infinite loop — see below) when run against a current `dev` build. The FIRST investigation pass
(same day, earlier) captured a stack dump showing the hang inside picocli's own `ArgGroupSpec`/`ColorScheme`/`Text`
CLI-help-synopsis-text building; a SECOND, deeper pass (using a custom timing-instrumented JUnit Platform runner,
see "Repro" below) found this was a snapshot of an early/transient phase, not the actual steady-state hang location
— the real hang is in **SmallRye Config's `RelocateConfigSourceInterceptor`**, and it is a genuine (if severe)
**interpreter-throughput problem, not an infinite loop or correctness bug**.

### Bisection: confirmed pre-existing, not a regression

Built a binary at `058e2b957` (the commit immediately before `10a561f21`, the SmallRye-config-resolution fix — see
the sibling doc) and reran the identical repro: **still hangs** (300.9s wall, `HANG: 1`). The hang is NOT introduced
by `10a561f21`'s native-override removals; it pre-dates that commit.

### Root cause: `RelocateConfigSourceInterceptor.getValue()` calls `context.proceed()` TWICE per invocation

SmallRye Config 3.16.0's `RelocateConfigSourceInterceptor` (see
`io/smallrye/config/RelocateConfigSourceInterceptor.java`):

```java
public ConfigValue getValue(final ConfigSourceInterceptorContext context, final String name) {
    String map = getMapping().apply(name);
    ConfigValue relocateValue = context.proceed(map);      // proceed #1 (relocated name)
    if (name.equals(map)) { return relocateValue; }
    ConfigValue configValue = context.proceed(name);        // proceed #2 (original name)
    ...
}
```

Every relocate interceptor calls `context.proceed()` twice against the SAME downstream chain (once for the
relocated name, once for the original). Quarkus/Keycloak registers a chain of MULTIPLE stacked
`RelocateConfigSourceInterceptor` instances (one per legacy-property-relocation source across the various
extensions). Since each layer's `proceed()` call reaches the NEXT relocate interceptor (which itself calls
`proceed()` twice more), a single property-value resolution against N stacked relocate interceptors costs up to
`O(2^N)` total interceptor invocations — this is legitimate SmallRye behavior, not a bug, and is normally
negligible because each individual call is nanoseconds under a JIT.

A captured thread dump (via a custom timing-instrumented runner, `httpAccessLog`'s hang point) shows exactly this
pattern repeating at depth 90-108 of a 109-frame stack:

```
RelocateConfigSourceInterceptor.getValue → SmallRyeConfigSourceInterceptorContext.proceed
→ RelocateConfigSourceInterceptor.getValue → ...proceed → RelocateConfigSourceInterceptor.getValue → ...
```

### Confirmed NOT an infinite loop: JIT-allowing `io/smallrye/config/` measurably fixes individual test methods

`picocli/`, `org/keycloak/`, and `io/smallrye/` are ALL forced onto CratonVM's interpreter under the default
conservative JIT policy (`../../../../vm/src/jit/skip_list.rs`, KC26-PIC.1 ban, 2026-07-05). Running the SAME test sequence with
`CRATONVM_JIT_ALLOW_PACKAGES=io/smallrye/config/,org/keycloak/quarkus/runtime/configuration/`:

- `httpAccessLog` (the test that hung past 180s without the override) completed in **41.8s** with it.
- The exponential `2^N` interceptor-chain cost is tractable under JIT (nanoseconds/call) but not under a pure
  bytecode interpreter (each call carries full interpreter dispatch overhead), which is exactly the kind of
  workload shape (many short-lived, low-iteration-count generated/lambda call sites) that amortizes JIT compilation
  cost poorly — consistent with genuinely large-but-finite work, not a non-terminating loop.

**However**, the test immediately AFTER `httpAccessLog` (`otelLogs`) STILL hangs past 180s even with the JIT-allow
override — meaning either later tests exercise substantially more property-relocation fan-out than earlier ones
(plausible — HTTP/telemetry/logging options are likely to have accumulated the most legacy-property relocations
across Quarkus/Keycloak version history), or there is a still-unidentified accumulation/growth pattern across
consecutive test invocations in the same process (not yet distinguished — see Next steps).

### IMPORTANT CAVEAT: do not naively lift the `io/smallrye/`/`picocli/`/`org/keycloak/` JIT skip-list

The skip-list ban this hang runs into was added 2026-07-05 for the OPPOSITE-looking reason: at that time, DEFAULT
(unrestricted) JIT was empirically SLOWER for this same test class (265s timeout) than forcing these packages
interpreted (`--nojit` completed in ~172s; `CRATONVM_JIT_DENY=org/keycloak/,picocli/,io/smallrye/` completed in
~168s). This is not necessarily a contradiction — commit `10a561f21` (and likely others between 2026-07-05 and now)
changed which native fast-paths short-circuit which bytecode paths, so the actual code being interpreted-vs-JIT'd
today differs substantially from 2026-07-05's measurement. But it means the 2026-07-05 finding could easily still
apply to OTHER tests/call-shapes in the same class even if it no longer applies to the specific
`RelocateConfigSourceInterceptor` hot loop measured here. **Do not blanket-lift the skip-list ban without
re-running the full `PicocliTest` class (and ideally the broader `quarkus/runtime` module) both with and without
the override to confirm no regression** — the safer fix is almost certainly a narrow, targeted addition (either a
carve-out in the skip-list scoped to just `RelocateConfigSourceInterceptor`/`SmallRyeConfigSourceInterceptorContext
.proceed`, matching the existing `org/keycloak/models/credential/` carve-out pattern already in
`skip_list.rs`, or a native fast-path for this specific hot method pair, matching the established pattern used by
the 2026-07-05 fix itself).

## Why this is NOT the same root cause as the SmallRye config-resolution-mismatches doc

The already-landed SmallRye-config fix (`10a561f21`, see the sibling doc) does not touch
`RelocateConfigSourceInterceptor`, the JIT skip-list, or any picocli code — it fixed a different class of bug
(stale/placeholder `Object` references leaking out of native collection/stream operations). This hang is a pure
interpreter-throughput problem in a completely different subsystem (SmallRye's relocate-interceptor chain
traversal), confirmed unrelated by the bisection above (hangs identically before and after `10a561f21`).

## Fix landed (2026-07-13, third pass): KC26-PIC.2 JIT skip-list carve-out

`../../../../vm/src/jit/skip_list.rs`'s KC26-PIC.1 ban (`org/keycloak/`, `picocli/`, `io/smallrye/` forced interpreted under
the Conservative policy) now has a narrow carve-out for:
- `io/smallrye/config/` (the SmallRye interceptor chain itself — `RelocateConfigSourceInterceptor`,
  `SmallRyeConfigSourceInterceptorContext`, `ExpressionConfigSourceInterceptor`, etc.)
- `org/keycloak/quarkus/runtime/configuration/` (Keycloak's own `PropertyMappingInterceptor`/
  `NestedPropertyMappingInterceptor`/`PropertyMapper` layer, which is interleaved into the same recursive
  `proceed()` chain)
- `org/keycloak/quarkus/runtime/cli/` (`Picocli.java`'s own `validateConfig`/`validateProperty` orchestration loop,
  which iterates once per registered CLI option and was a SECOND bottleneck once the interceptor calls themselves
  got fast)

`picocli/` itself (picocli's own library code — `CommandLine`, `ArgGroupSpec`, `Help`, etc.) and the REST of
`org/keycloak/` (models, services, everything outside `.../configuration/` and `.../cli/`) remain interpreted —
this is a deliberately narrow carve-out, not a lift of the whole ban.

**Verified**:
- Zero regressions: `DatasourcesConfigurationTest` (33/33), `TracingConfigurationTest` (13/13),
  `IgnoredArtifactsTest` (15/15), `ConfigurationTest` (73/73) all still PASS cleanly with the carve-out
  (`verify-regression-relocatefix-20260713`).
- Two new unit tests added to `skip_list.rs`: `kc26_pic2_smallrye_relocate_carveout_lifted_under_conservative`
  (asserts the carved-out classes ARE JIT-eligible, and that `picocli/`/the rest of `org/keycloak/` are NOT) and an
  updated `keycloak_picocli_smallrye_packages_skip_under_conservative` (swapped its `org/keycloak/`/`io/smallrye/`
  example classes to ones OUTSIDE the new carve-out, to keep covering the general ban).
- Iteratively widened against real stack dumps: started with just `io/smallrye/config/` +
  `.../configuration/` (`httpAccessLog`: >180s → 41.8s single-test; full class: hung at test ~20 → test 36/107),
  then added `.../cli/` after a fresh dump showed the NEXT bottleneck was `Picocli.validateProperty`'s per-option
  loop rather than the interceptor chain itself (full class: 36/107 → 55/107 before stalling).

**NOT fully fixed**: the full `PicocliTest` class (107 tests) still does not complete within a 300-400s window even
with this carve-out, AND — critically — even with the ENTIRE original ban fully lifted via
`CRATONVM_JIT_ALLOW_PACKAGES=org/keycloak/,picocli/,io/smallrye/` (i.e. `picocli/` JIT-eligible too), it STILL hangs
past 400s (`verify-picocli-fullallow-20260713`). This means the remaining bottleneck, for at least one test's
specific CLI options, is not solvable by JIT-eligibility alone — either the fan-out for that specific property's
relocation chain is large enough that even JIT-speed `2^N` calls don't finish in reasonable time, or there's a
genuinely different (possibly still-unidentified accumulation) issue for later tests. This is real, verified
partial progress, not a complete fix — treat the class as still HANG-prone in full-suite runs.

## Next steps

1. Get a fresh stack dump for wherever the class stalls now (`wildcardLevelFromParent` at last check, but this may
   shift as the fix evolves) using the TimedKcRunner technique in "Repro" below, to see if it's still the same
   `RelocateConfigSourceInterceptor` pattern (now just needing a deeper/larger-N case) or something new.
2. Count how many `RelocateConfigSourceInterceptor` instances are actually stacked in Keycloak's real
   `SmallRyeConfigBuilder` chain (add a one-off diagnostic print in `KeycloakConfigSourceProvider` or wherever the
   chain is assembled) to get the real `N` and confirm/refute the `2^N` fan-out theory quantitatively — if `N` is
   large enough (30-40+) even JIT-speed calls could genuinely take minutes, which would mean the real fix has to
   reduce the FAN-OUT itself (e.g. memoizing `RelocateConfigSourceInterceptor.getValue()` per name within a single
   property resolution, since SmallRye's own semantics don't obviously require re-doing the full downstream
   traversal twice when `map == name` is common) rather than just making the interpreter faster.
3. Consider whether a native fast-path for `RelocateConfigSourceInterceptor.getValue()`/
   `SmallRyeConfigSourceInterceptorContext.proceed()` (bypassing bytecode entirely for this specific hot pair,
   matching the established pattern from the 2026-07-05 fix) would help further where the JIT-eligibility carve-out
   alone doesn't — a native implementation could also add memoization within a single top-level resolution that
   Java-level `proceed()` semantics can't easily add without changing SmallRye's own source.

## Repro

Full class (via harness, ~41KB module classpath needs a pathing jar under Windows — see below):
```
cd C:\craton\CratonVM
$jdk = "C:\Program Files\Java\jdk-25"
& .\apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 300 -Parallel 1 -RunName repro-picocli-hang -ClassList <classlist with only quarkus/runtime org.keycloak.quarkus.runtime.cli.PicocliTest> -Exe <dev build> -JdkHome $jdk
```

To see exactly which test is executing when the hang occurs (JUnit4's default test order isn't obvious from the
outside), use a custom JUnit Platform runner with a `TestExecutionListener` that timestamps `executionStarted`/
`executionFinished` per test to stderr — copy `../../../../apps/keycloak/kc-runner/KcRunner.java`'s `main()`, add the listener,
compile into the SAME `kc-runner` directory (already on the harness's cached pathing-jar Class-Path), then build a
NEW pathing jar reusing the harness's cached Class-Path but with `Main-Class: <YourRunner>` in its manifest instead
of `KcRunner` (the raw classpath is too long for a direct command line on Windows — `CreateProcess`'s ~32,767
character limit — hence the pathing-jar/manifest-`Class-Path` indirection; building one is cheap: extract
`../../../../apps/META-INF/MANIFEST.MF` from an existing cached jar under
`apps\keycloak-suite-runner\.suite\pathing-jars\quarkus_runtime-*.jar`, `sed` the `Main-Class:` line, re-jar).

To also get a clean stack dump when it hangs: the harness itself always passes `--stack-dump-on-timeout 0`, which
DISABLES CratonVM's internal watchdog in favor of the harness's own external kill (no dump). Invoke the pathing jar
manually instead with `--stack-dump-on-timeout <N>` for a real dump.

To test the JIT-allow mitigation: set `$env:CRATONVM_JIT_ALLOW_PACKAGES =
"io/smallrye/config/,org/keycloak/quarkus/runtime/configuration/"` before invoking (PowerShell env vars propagate
to child `ProcessStartInfo`-launched processes by default, including through the harness script).

## Evidence

- First-pass stack dump (ArgGroupSpec/ColorScheme/Text, now understood to be a transient early phase, not the
  steady-state hang): 2026-07-13, worktree `C:\data\CratonVM-quarkusconfig-verify-20260713` at commit `10a561f21`.
- Bisection: worktree `C:\data\CratonVM-picocli-bisect-20260713`, branch `bisect/picocli-pre-10a561f21-20260713`,
  commit `058e2b957`, run `verify-picocli-bisect-prefix-20260713` — HANG, 300.9s wall.
- Timed-runner traces showing per-test elapsed times and the `RelocateConfigSourceInterceptor` recursive stack
  dump: local captures `timedrun_utf8.log` / `timedrun_jitallow_utf8.log` (not preserved as repo files — see the
  "Repro" section to regenerate), 2026-07-13, worktree `C:\data\CratonVM-quarkusconfig-verify-20260713` at `dev`
  commit `10a561f21`.
