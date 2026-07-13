# quarkus/runtime PicocliTest hang inside picocli ArgGroupSpec synopsis text building

Status: open

Date observed: 2026-07-13, split out from
`docs/internal/fixed-suite-bugs/keycloak-quarkus-runtime-config-resolution-mismatches.md`, whose 2026-07-13 update
speculatively grouped this with SmallRye Config resolution mismatches ("may share a root cause"). That hypothesis
is refuted below — this is a separate, unrelated bug.

## Summary

`quarkus/runtime :: org.keycloak.quarkus.runtime.cli.PicocliTest` genuinely HANGS (does not merely run slowly) when
run via the keycloak-suite-runner harness against a current `dev` build. Confirmed via two independent methods:

1. Two harness runs at different `-TimeoutSec` (180s and 300s) both cut off at the identical point in stderr
   (`DEBUG [io.netty.util.NetUtil] Failed to get SOMAXCONN from sysctl and file \proc\sys\net\core\somaxconn.
   Default: 200`), with zero further output either time — if it were merely slow, the 300s run should have printed
   more before its cutoff than the 180s run did.
2. A manually-launched process (bypassing the harness) showed **flat CPU time** across a 20s window (1.796875s of
   CPU time both at the 20s and 40s checks) — the process was alive but making zero forward progress, not just
   running slowly.
3. CratonVM's own `--stack-dump-on-timeout` watchdog (armed at 35s, via a manually-constructed pathing jar with
   `Main-Class: KcRunner` reusing the harness's own cached module classpath — the raw classpath is ~41KB, over
   Windows' ~32K `CreateProcess` command-line limit, hence the pathing-jar workaround) produced a full thread dump
   showing the single `main` thread 58 frames deep in:

   ```
   org.keycloak.quarkus.runtime.cli.PicocliTest.otelLogsHeaders
   → PicocliTest.pseudoLaunch → NonRunningPicocli.launch → KeycloakMain.main
   → Picocli.parseAndRun → Picocli.addCommandOptions → Picocli.addMappedOptionsToArgGroups
   → picocli.CommandLine$Model$ArgGroupSpec$Builder.build()
   → picocli.CommandLine$Model$ArgGroupSpec.<init>
   → picocli.CommandLine$Model$ArgGroupSpec.synopsisUnit()
   → picocli.CommandLine$Model$ArgGroupSpec.rawSynopsisUnitText()
   → picocli.CommandLine$Model$ArgGroupSpec.concatOptionText()
   → picocli.CommandLine$Help.concatOptionText()
   → picocli.CommandLine$Help$ColorScheme.optionText()
   → picocli.CommandLine$Help$ColorScheme.apply()
   → picocli.CommandLine$Help$Ansi$Text.<init>()
   → picocli.CommandLine$Help.defaultColorScheme()
   → picocli.CommandLine$Help$ColorScheme$Builder.build()
   → picocli.CommandLine$Help$ColorScheme.<init>()
   ```

   i.e. it's stuck constructing the ANSI-styled command-line help synopsis text for the `start-dev` command's
   `ArgGroupSpec`s — this happens as a SIDE EFFECT of building the `CommandLine` spec via
   `Picocli.addCommandOptions`, well BEFORE any SmallRye config-source or interceptor code is reached. This is the
   VERY FIRST `pseudoLaunch(...)` call inside `PicocliTest.otelLogsHeaders()` (itself apparently one of the first
   methods JUnit4's default method-sorter picks for this class).

## Why this is NOT the same root cause as the SmallRye config-resolution-mismatches doc

- The stuck frame is 100% inside `picocli.*` internals building help-text synopsis strings — no
  `io.smallrye.config.*` or `org.keycloak.quarkus.runtime.configuration.*` frame appears anywhere in the 58-frame
  dump.
- `picocli/` packages are deliberately excluded from CratonVM's JIT allow-list under the conservative default
  policy (see `vm/src/jit/skip_list.rs`, `keycloak_picocli_smallrye_packages_skip_under_conservative` test) — so
  this is running purely interpreted, which is consistent with either (a) genuinely slow O(n²)-or-worse interpreted
  text-concatenation for a CLI with hundreds of options across many `ArgGroup`s, or (b) an actual infinite loop
  somewhere in this text-building chain that real HotSpot's JIT would make appear instantaneous even if it were
  doing wasted repeated work, but CratonVM's interpreter cannot outrun.
- The already-landed SmallRye-config fix (`10a561f21`, see the sibling doc) does NOT touch anything in this call
  chain — no picocli natives, no `ArgGroupSpec`/`ColorScheme`/`Text` code was changed.

## Historical context

A previous, apparently DIFFERENT hang in this same test class was fixed 2026-07-05 (see
`docs/internal/fixed-suite-bugs/quarkus-runtime-picocli-post-compactvalue-hang.md`) — that hang's last-seen line
before timeout was the `ExecutionExceptionHandler` `TlsUtils` WARN, i.e. it hung BEFORE even reaching Netty
initialization, whereas this hang gets substantially further (through Netty init and into
`KeycloakMain.main`/`Picocli.parseAndRun`) before getting stuck — this is not a regression of that same bug, since
this hang point is strictly later in the startup sequence. It's plausible removing/changing some of the "native
fast paths for Picocli and Keycloak configuration helpers" mentioned in that 2026-07-05 fix (as part of the
2026-07-13 config-resolution fix, which removed `PropertyMappingInterceptor.hasInferredValue` and a few other
native overrides) exposed this DIFFERENT, deeper hang that a removed fast-path was previously short-circuiting —
but this has not been confirmed; it may equally be a pre-existing, never-exercised-until-now performance gap in
picocli `ArgGroupSpec` synopsis building.

## Next steps

1. Bisect whether this hang is NEW (introduced by commit `10a561f21`'s removal of
   `PropertyMappingInterceptor.hasInferredValue`/`TracingPropertyMappers.isTracingEnabled`/etc. native overrides)
   or pre-existing — build a binary at the commit immediately BEFORE `10a561f21` and rerun the same repro.
2. If pre-existing: profile/trace `picocli.CommandLine$Model$ArgGroupSpec.rawSynopsisUnitText()`/`concatOptionText()`
   for an actual infinite loop (e.g. a `Text` concatenation building an ever-growing structure without terminating)
   vs. genuinely-large-but-finite interpreted work — the historical fix doc's approach (native fast paths for hot
   picocli/Keycloak CLI helper methods) may need extending to cover whatever specific method(s) this deeper call
   chain exercises that weren't covered before.
3. Consider whether allow-listing just the specific `picocli.CommandLine$Help$*`/`ArgGroupSpec` classes for JIT
   (via `CRATONVM_JIT_ALLOW_PACKAGES`) makes the hang complete in reasonable time — if so, this narrows the
   diagnosis to "genuinely slow interpreted execution" rather than a true infinite loop, and the fix becomes a
   targeted native fast path (matching the established pattern in this codebase) rather than a correctness bug fix.

## Repro

```
cd C:\craton\CratonVM
$jdk = "C:\Program Files\Java\jdk-25"
& .\apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 300 -Parallel 1 -RunName repro-picocli-hang -ClassList <classlist with only quarkus/runtime org.keycloak.quarkus.runtime.cli.PicocliTest> -Exe <dev build> -JdkHome $jdk
```

To get a clean stack dump (classpath is too long for a direct command line — build a pathing jar first, reusing
the harness's own cached one at `apps\keycloak-suite-runner\.suite\pathing-jars\quarkus_runtime-*.jar` but with
`Main-Class: KcRunner` in its manifest, then pass `--jar <pathing-jar> org.keycloak.quarkus.runtime.cli.PicocliTest`
with `--stack-dump-on-timeout <N>` — the harness itself always passes `--stack-dump-on-timeout 0`, which DISABLES
CratonVM's internal watchdog in favor of the harness's own external kill, so a stack dump requires a manual
invocation like this).

## Evidence

`C:\craton\CratonVM\apps\keycloak-suite-runner\.suite\results\verify-picocli-retry-20260713\all-jit\logs\` (180s and
300s hang cutoffs) and a manual stack-dump capture (not preserved as a file — 7.5MB, watchdog fired at 35s, single
thread 58-frame trace as quoted above), 2026-07-13, local Windows-box build from worktree
`C:\data\CratonVM-quarkusconfig-verify-20260713` at `dev` commit `10a561f21`.
