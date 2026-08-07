# G1 vs. Generational — first full Spring Boot suite comparison, 2026-08-07

**Status: OPEN — characterized, not root-caused.** Same methodology as the
companion ZGC-real comparison run the same day
([`zgc-real-fullsuite-regression-20260807.md`](zgc-real-fullsuite-regression-20260807.md)):
same binary, same 1975-class Windows-box full suite, same 4-way shard split,
`-Xmx 2g`, 300s/class timeout, only `-XX:+UseG1GC` vs. the default
(unspecified → Generational) varies.

## Summary

| | Generational (default) | G1 (`-XX:+UseG1GC`) |
|---|---:|---:|
| PASS | 1902 (96.3%) | 1896 (96.0%) |
| HANG | 18 | 14 |
| FAIL | 11 | 20 |
| CRASH | 0 | 1 |
| EMPTY | 43 | 43 |
| BOTH-FAIL | 1 | 1 |
| **Total** | **1975** | **1975** |

EMPTY and BOTH-FAIL are identical on both arms, as expected (neither should
be collector-sensitive). Only **14 classes changed status** — far fewer than
ZGC-real's 50, consistent with G1 being "wired into the safepoint driver"
(the more mature of the two non-default backends per
`vm-cli/src/main.rs`'s own selector comment) rather than ZGC-real's
documented "non-moving, whole-heap stop-the-world... research vehicle"
status.

Results:
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-windows-20260806-s{1..4}/all-jit/results.tsv`
(Generational) vs.
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-g1-20260807-s{1..4}/all-jit/results.tsv`
(G1).

## The headline finding: 8 of G1's 9 new FAILs are the *same 8 classes* ZGC-real also newly fails

```
core/spring-boot                             BindConverterTests
loader/spring-boot-loader                    VirtualZipDataBlockTests
module/spring-boot-jetty                     JettyReactiveWebServerFactoryTests
module/spring-boot-micrometer-metrics        PropertiesMeterFilterTests
module/spring-boot-micrometer-tracing-brave  OtlpExemplarsAutoConfigurationTests
module/spring-boot-micrometer-tracing-brave  PrometheusExemplarsAutoConfigurationTests
module/spring-boot-reactor-netty             NettyReactiveWebServerFactoryTests
module/spring-boot-tomcat                    TomcatReactiveWebServerFactoryTests
```

These 8 pass under Generational and fail under **both** alternate
collectors — strong evidence this is a **collector-agnostic** bug (something
that assumes/relies on Generational-specific behavior and breaks under *any*
non-default backend), not a G1-specific or ZGC-specific defect. Checked one
directly: `VirtualZipDataBlockTests` fails with the byte-for-byte
**identical** signature under both G1 and ZGC-real — same
`NoSuchFileException`, same `AssertionFailedError` with the same missing
`META-INF/` entry bytes (`[77, 69, 84, 65, 45, 73, 78, 70, 47]`) in the same
position. That rules out coincidence for at least this one; the other 7
weren't individually diffed G1-vs-ZGC this round. See the ZGC doc's own "PASS
-> FAIL" section for the parallel list (11 there, 8 of which are this set;
the ZGC-only 3 are `ServletListenerRegistrationBeanTests`,
`DataNeo4jAutoConfigurationTests`, `SqlDialectLookupTests`).

G1's one 9th FAIL not in the ZGC set:
`module/spring-boot-actuator-autoconfigure`'s
`ChildManagementContextInitializerAotTests` — not cross-checked against ZGC
individually (it wasn't in ZGC's diff list at all, i.e. it passed under
ZGC), so this one may be genuinely G1-specific rather than part of the
shared-8 family.

## The other finding: a CRASH with a self-diagnosed likely cause

`module/spring-boot-actuator`'s
`ConfigurationPropertiesReportEndpointSerializationTests` — clean PASS under
Generational, **`EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005)`** under
G1 at 16.4s. The VM's own fatal-error report names a specific, plausible
mechanism:

```
#  gc collector: g1
#  gc young-gen policy: moving (Cheney young copy)
#  gc young-gen actual: 0 moving cycle(s), 0 cycle(s) diverted to the NON-MOVING sweep
#  gc young-gen last incomplete-coverage reason: unregistered-jit-frame-on-stack
#  gc young-gen: the faulting thread had an UNREGISTERED JIT frame on its native stack
#    in the last root-gathering pass (no precise root map for it)
```

Log:
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-g1-20260807-s2/all-jit/logs/module_spring-boot-actuator.org.springframework.boot.actuate.context.properties.-f819d00d97e6.err.log`

**This is very likely the same general defect family as an already-FIXED
bug, but not the same fixed site.**
[`gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md`](../internal/fixed-suite-bugs/app-jvm-bugs/gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md)
closed an unregistered-JIT-frame root-coverage gap for the Generational
collector's moving young-gen (a frame invoked without a `JitEntryGuard`
gets missed by `gc_quiescence`, so the collector relocates instead of
running a non-moving sweep to protect it) — the fix detects the unregistered
frame and **diverts to the non-moving sweep** when one is found. That doc's
own 2026-06-22 update further found, at the time, **Generational crashed and
G1 was clean** for that specific repro — the opposite collector-safety
ordering from what this new crash shows. Two readings are both consistent
with the data:

1. The diversion fix that doc landed is Generational-specific machinery and
   was never extended to G1's own moving young-gen path, so G1 has its own,
   separate instance of the same class of gap.
2. This is a different concrete trigger within the same "GC root coverage
   under JIT" family (the crash log's own "last incomplete-coverage reason"
   field is a general diagnostic, not proof of identical mechanism) that
   happens to reproduce specifically under G1's collector, the way the
   original bug reproduced specifically under Generational.

**Not disambiguated — not root-caused, just characterized.** The crash
report's `gc young-gen actual: 0 moving cycle(s), 0 cycle(s) diverted`
line says the diversion-to-non-moving-sweep counter is at zero for this
run, which is consistent with reading #1 (G1 never runs that diversion
logic at all) but was not independently confirmed by, e.g., grepping the G1
collector's own code for whether it calls the same detection routine
Generational's fix added.

## 4 classes: HANG (Generational) -> PASS (G1)

```
module/spring-boot-jooq-test  JooqTestIntegrationTests
module/spring-boot-jooq-test  JooqTestPropertiesIntegrationTests
module/spring-boot-jooq-test  JooqTestWithAutoConfigureTestDatabaseIntegrationTests
module/spring-boot-webflux    WebFluxAutoConfigurationTests
```

Three of four are jOOQ-family — the exact same family and likely the exact
same "timeout-boundary noise" explanation already flagged in the ZGC doc's
parallel section (that doc's 4-class HANG->PASS list is 3 of these same
jOOQ classes plus a 4th jOOQ class this list doesn't have). Not
investigated further; plausibly not a genuine G1 advantage.

## Not done this round

- Neither the 8 shared-with-ZGC FAILs (beyond the one `VirtualZipDataBlockTests`
  cross-check) nor `ChildManagementContextInitializerAotTests` were
  individually root-caused.
- The crash's two competing readings above were not disambiguated (would
  need reading G1's collector source for the same detection call the
  Generational fix added, or reproducing under
  `CRATONVM_DBG_JIT_NAMES=1` to get a symbolized faulting frame — this run's
  report explicitly notes "JIT method names are off").
- No re-run for reproducibility on either arm.

## Affected classes

See the sections above (8 shared-with-ZGC FAILs, 1 G1-only FAIL, 1 CRASH, 4
HANG->PASS flips). Full per-class raw data in the results.tsv files linked
at the top.
