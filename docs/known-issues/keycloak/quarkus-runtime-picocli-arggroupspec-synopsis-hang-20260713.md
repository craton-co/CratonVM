# quarkus/runtime PicocliTest hang — exponential-ish RelocateConfigSourceInterceptor fan-out under the JIT skip-list

Status: open

Date observed: 2026-07-13, split out from
`docs/internal/fixed-suite-bugs/keycloak-quarkus-runtime-config-resolution-mismatches.md`, whose 2026-07-13 update
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
conservative JIT policy (`vm/src/jit/skip_list.rs`, KC26-PIC.1 ban, 2026-07-05). Running the SAME test sequence with
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

## Next steps

1. Determine whether the growing per-test cost (`httpAccessLog` 41.8s → `otelLogs` still >180s, both under the same
   JIT-allow override) is (a) inherent to those specific tests' CLI options touching more legacy-relocated
   properties, or (b) a genuine accumulation/leak across `PropertyMappers.reset()`/`Configuration.resetConfig()`
   cycles. `Configuration.resetConfig()` itself looks clean (`config = null` + `KeycloakConfigSourceProvider.reload()`
   — no obviously-growing collection), and `System.setProperties()`'s native implementation
   (`native-builtins/src/lib.rs` ~line 27676) already does a correct full-replace (removes all old keys before
   setting new ones, with an explicit comment noting this was fixed for exactly this
   `AbstractConfigurationTest`/`System.setProperties(clone)` reset pattern) — so system-property accumulation is
   ruled out. The `KeycloakConfigSourceProvider`/interceptor-chain construction itself has not yet been audited for
   growth.
2. Count how many `RelocateConfigSourceInterceptor` instances are actually stacked in Keycloak's real
   `SmallRyeConfigBuilder` chain (add a one-off diagnostic print in `KeycloakConfigSourceProvider` or wherever the
   chain is assembled) to get the real `N` and confirm the `2^N` fan-out theory quantitatively rather than just
   from the stack-dump shape.
3. Implement either: (a) a narrow JIT skip-list carve-out for `RelocateConfigSourceInterceptor`/
   `SmallRyeConfigSourceInterceptorContext.proceed`, verified against the full `quarkus/runtime` module for
   regressions, or (b) a native fast-path for the same hot pair, matching this codebase's established pattern for
   this exact class of problem (see the 2026-07-05 fix's own "native fast paths for Picocli and Keycloak
   configuration helpers").

## Repro

Full class (via harness, ~41KB module classpath needs a pathing jar under Windows — see below):
```
cd C:\craton\CratonVM
$jdk = "C:\Program Files\Java\jdk-25"
& .\apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 300 -Parallel 1 -RunName repro-picocli-hang -ClassList <classlist with only quarkus/runtime org.keycloak.quarkus.runtime.cli.PicocliTest> -Exe <dev build> -JdkHome $jdk
```

To see exactly which test is executing when the hang occurs (JUnit4's default test order isn't obvious from the
outside), use a custom JUnit Platform runner with a `TestExecutionListener` that timestamps `executionStarted`/
`executionFinished` per test to stderr — copy `apps/keycloak/kc-runner/KcRunner.java`'s `main()`, add the listener,
compile into the SAME `kc-runner` directory (already on the harness's cached pathing-jar Class-Path), then build a
NEW pathing jar reusing the harness's cached Class-Path but with `Main-Class: <YourRunner>` in its manifest instead
of `KcRunner` (the raw classpath is too long for a direct command line on Windows — `CreateProcess`'s ~32,767
character limit — hence the pathing-jar/manifest-`Class-Path` indirection; building one is cheap: extract
`META-INF/MANIFEST.MF` from an existing cached jar under
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
