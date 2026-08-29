# G1 vs. Generational, full Spring Boot suite — RETIRED 2026-08-07

**Status: CLOSED.** The page's headline finding is root-caused and fixed; its
two competing readings of the crash are both refuted, one of them by an
experiment the page asked for and could not run; and every remaining item is
either fixed elsewhere or is not a collector regression.

Original page: the 2026-08-07 `g1-fullsuite-regression-20260807` write-up
(1975-class Windows full suite, same binary, `-XX:+UseG1GC` vs. the default).
Its numbers are unchanged and not restated here; this record says what each of
its findings turned out to be.

## 1. The 8 classes that failed under BOTH alternate collectors — FIXED

**Root cause: G1 dropped every primitive stored into a REFERENCE array.**

A reference array element is a raw 8-byte pointer, so a non-Object `Value`
cannot be stored in one directly. Natives across the tree nonetheless use a
reference array as a generic `Value` store, and both `GenerationalHeap` and
`Heap` have always honoured that by auto-boxing into a one-field
`AUTOBOX_CLASS_ID` wrapper and un-boxing on read. G1 implemented neither half —
its encoder was

```rust
Value::Object(Some(r)) => r.as_ptr() as u64,
_ => 0u64,
```

so a `Value::Long(42)` was written as a null reference and read back as
`Value::Object(None)`.

The stream pipeline is the loudest victim, because `mapToLong`/`mapToDouble`
collect primitives into exactly such an array:

```
Arrays.stream(boxed).mapToDouble(Double::doubleValue).toArray()
  default collector -> [1.0, 2.5, 3.75, 100.0]
  -XX:+UseG1GC      -> [0.0, 0.0, 0.0, 0.0]
```

`PropertiesMeterFilter.convertServiceLevelObjectives` is character-for-character
that expression, which is why `PropertiesMeterFilterTests` failed with
*"serviceLevelObjectiveBoundaries must contain only the values greater than 0.
Found 0.0"*.

Three properties of the repro are worth keeping, because each one contradicts
how the page framed the family:

* **It is not a collection-time defect.** Identical with `--nojit` and
  identical at `--Xmx 6g`, where no collection runs at all. The page's
  "collector-agnostic bug" framing pointed at something that "assumes
  Generational-specific behavior"; the actual answer is narrower and duller —
  two backends never implemented one half of a store contract.
* **4-byte elements survived.** `mapToInt(...).toArray()` was correct, which is
  what made this look like a width or layout bug rather than a missing match arm.
* **A stream built straight from a primitive array was correct too**
  (`DoubleStream.of(...).toArray()`), because it never goes through the
  reference-array store. That is the control.

Regression coverage: `probes/G1ReferenceArrayValueProbe.java`
(`PROBE-FAILURES=6` -> `PROBE-OK`), plus three unit tests in `gc/src/g1.rs`
(`reference_array_round_trips_every_value_kind`,
`primitive_arrays_are_not_auto_boxed`,
`an_ordinary_object_is_not_mistaken_for_an_auto_box`).

### Measured on the 14 classes the page named

Azure Linux host, dev tip merged, 4-way parallel, 300 s per class, two runs per
arm, ABBA-interleaved. `g1pin` is the new diagnostic lever described in §3.

| class | Generational | G1 before | G1 after |
|---|---|---|---|
| `VirtualZipDataBlockTests` | PASS | FAIL | **PASS** |
| `JettyReactiveWebServerFactoryTests` | PASS | FAIL | **PASS** |
| `PropertiesMeterFilterTests` | PASS | FAIL | **PASS** |
| `OtlpExemplarsAutoConfigurationTests` | PASS | FAIL | **PASS** |
| `PrometheusExemplarsAutoConfigurationTests` | PASS | FAIL | **PASS** |
| `NettyReactiveWebServerFactoryTests` | PASS | FAIL | **PASS** |
| `BindConverterTests` | PASS | PASS | PASS |
| `TomcatReactiveWebServerFactoryTests` | FAIL | FAIL | FAIL |
| `ConfigurationPropertiesReportEndpointSerializationTests` (the CRASH) | unstable | unstable | unstable |
| `ChildManagementContextInitializerAotTests` | HANG | FAIL | FAIL |
| 3 × `JooqTest*IntegrationTests` | PASS | PASS | PASS |
| `WebFluxAutoConfigurationTests` | PASS | PASS | PASS |

Six of the eight are fixed. The remaining two are **not** collector
regressions:

* `BindConverterTests` already passed under G1 at dev tip, before this fix.
* `TomcatReactiveWebServerFactoryTests` fails **under the default collector
  too** on this host, with `ConnectorStartFailedException: Connector configured
  to listen on port 8080 failed to start` — the Azure box runs many agents and
  8080 is occupied. So it is not evaluable here, and its Windows-side failure
  is neither confirmed nor explained by this run. Flagged, not claimed.

## 2. `ChildManagementContextInitializerAotTests` — not a G1 regression

The page recorded it as G1's one FAIL outside the shared set and asked whether
it was genuinely G1-specific. It is not: under the **default** collector it
TIMEOUTs (400 s, 2/2 runs, no result line at all), and under G1 it fails fast
and deterministically (2/2) with a Spring property-binding error —
`Could not bind properties to 'WebEndpointProperties'`. The default collector is
the worse arm. Whatever this class is hitting, changing the collector is not
what introduced it.

## 3. The CRASH, and both of the page's readings — REFUTED

`ConfigurationPropertiesReportEndpointSerializationTests` **never crashed
again** on any arm, in any run. What it does do is fail *unstably and
symmetrically*: PASS on all three arms twice each mid-afternoon, then FAIL
13/15 on **both** collectors on the final landing tree, and FAIL under the
default collector on the earlier binary too when driven through a different
harness. Whatever that instability is, it is not a collector asymmetry and it
is not this page's `EXCEPTION_ACCESS_VIOLATION`. It wants its own page; it does
not keep this one open.

The page offered two readings of the crash and disambiguated neither.

**Reading #1 — "the diversion fix is Generational-specific machinery and was
never extended to G1's moving young-gen" — is wrong.** G1's analogue exists; it
is *pinning*, not diversion, and it is comprehensive. Every source of
conservative JIT roots is published into the pin set that
`G1Collector::jit_pinned_region_set` consumes, and a pinned region is excluded
from the collection set:

* the collection initiator's own frames — `memory::roots::collect_roots` →
  `gc_quiescence::add_pinned_jit_root`;
* every parked or blocked mutator's frames —
  `interpreter::update_root_snapshot` → `publish_pinned_jit_roots`;
* forcibly-frozen peers and their helper windows —
  `interpreter::pin_frozen_peer_roots_for_g1`;
* frozen peers' un-retired TLAB tails — `G1Collector::set_jit_tlab_skip_regions`.

The premise underneath the reading is also false. Each incomplete-coverage
reason still leaves the frame conservatively SCANNED: an unregistered frame's
whole band is scanned by `scan_active_jit_frames` before the flag is set, and a
precise entry always additionally gets `scan_compiled_frame_bands` (or the
whole-band `scan_one_frame` fallback when its metadata is untrustworthy), so an
unresolvable innermost RBP or a missing exact RBP still leaves the frame
covered. "Coverage incomplete" under G1 means *the roots are not REWRITABLE* —
the normal state whenever a thread is in compiled code — not *the roots were not
ENUMERATED*.

**The page's own supporting evidence does not support it either.** It read
`gc young-gen actual: 0 moving cycle(s), 0 cycle(s) diverted to the NON-MOVING
sweep` as consistent with "G1 never runs that diversion logic". Those are
`gc_quiescence`'s **generational** counters; they are 0 on every G1 run by
construction, whatever G1 does. A field that cannot vary is not evidence.

**Reading #2 — "a different trigger in the same GC-root-coverage family" — is
refuted empirically.** This branch adds the experiment the page wanted: an
opt-in lever, `CRATONVM_G1_COVERAGE_PIN`, that makes G1 force an EMPTY
collection set on any pause whose root set is recorded incomplete. Under it G1
moves nothing at all, so a failure that survives it cannot be caused by a
relocation the root set failed to cover. Run on all 14 classes before the
array fix, **every failing class failed identically under the lever** — the
whole family is untouched by it. (The lever's own cost is why it ships OFF: on
`probes/MovingYoungConcurrentProbe 6 400 2000` a run that needs ONE collection
takes 330 264 no-op pauses with it on, because a pause that frees nothing is
immediately re-triggered.)

## 4. The four jOOQ/WebFlux HANG -> PASS flips — dissolved

The page suspected timeout-boundary noise. All four now PASS on **both**
collectors, and the jOOQ classes complete in ~10 s where they previously took
200 s+ — the young-pause work that landed on dev as
`fix/gc-young-pause-20260807`. There is no flip left to explain.

## 5. What this branch added so the next comparison is cheaper

* `[GC] g1 root coverage: pauses=N incomplete=M (x%)` in the shutdown GC
  report, and the G1 decision record now carries the obligation that actually
  failed. It previously passed the constant `incomplete_reason::NONE`, so under
  G1 the record said `moving-backend-always-evacuates` and nothing else no
  matter what the root scan found.
* `CRATONVM_G1_COVERAGE_PIN` (declared, `CRATONVM_GC=g1-coverage-pin`) — the
  bisection lever above. Default OFF.
* `probes/G1ReferenceArrayValueProbe.java`.

## 6. Correction to a claim made while investigating

An intermediate commit on this branch asserted that
`gc_metrics::collector_decision_report` "had no caller anywhere in the tree".
That is wrong: `VmHeap::print_gc_summary` has emitted it all along, and the
grep that produced the claim covered `vm/`, `vm-cli/` and `gc/src/lib.rs` but
not `gc/src/vm_heap.rs`. A duplicate emission added on that basis has been
removed. What this branch contributes to the report is content, not the call.

## Related, not fixed here

* **ZGC-real has the same array defect, one half of it.** It implements the
  auto-box on write and has no un-box on read, so a primitive comes back as the
  wrapper object rather than the primitive. Not fixed, because
  `gc/src/zgc.rs` does not currently compile at all under `--features zgc`: 24
  errors, the header shrink having turned `kind`/`element_type`/`gc_flags` into
  methods while that file — being `cfg`'d out of the default build — was never
  updated, plus a bare syntax error (a missing `)`). Verified on a pristine
  `origin/dev`. Restoring that build is its own piece of work and belongs with
  the ZGC comparison page, which predicts the same 8 classes.
* `TomcatReactiveWebServerFactoryTests` — see §1.
* `ChildManagementContextInitializerAotTests` — see §2. Bad on both arms and
  worse on the default one; wants its own page.

---

## The original page, verbatim

# G1 vs. Generational — first full Spring Boot suite comparison, 2026-08-07

**Status: OPEN — characterized, not root-caused.** Same methodology as the
companion ZGC-real comparison run the same day
([`zgc-real-fullsuite-regression-RETIRED-20260807.md`](zgc-real-fullsuite-regression-RETIRED-20260807.md)):
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
`../../../vm-cli/src/main.rs`'s own selector comment) rather than ZGC-real's
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
`../../../../apps/META-INF/` entry bytes (`[77, 69, 84, 65, 45, 73, 78, 70, 47]`) in the same
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
[`gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md`](../../internal/fixed-suite-bugs/app-jvm-bugs/gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md)
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
