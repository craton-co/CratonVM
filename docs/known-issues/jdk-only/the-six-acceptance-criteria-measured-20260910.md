# The six `--jdk-only` acceptance criteria, measured — three met, one met only within the census's reach, one partial, one half

**Status:** measurement record, 2026-09-10. Nothing here is a fix.
**Why it exists:** `docs/feature-designs/jdk-only-mode.md` §11 is the definition
of done for this mode, and no page in the tree said where the mode stood against
it. Two places that *do* describe the remaining work disagree with each other
and cite three different numbers, none of which matches a dump (§8).
**How:** one `scripts/jdk-only-census.sh` run per policy on a release binary
built from `origin/dev` @ `4d7177840`, Temurin 25 Linux, plus two launcher
probes. Artifacts: `registry-{real,strict}.json`, `classes-{real,strict}.json`,
`report-{real,strict}.json`.

## The table

| # | criterion | verdict |
|---|---|---|
| 1 | cannot start without a valid real JDK image | **MET** |
| 2 | final native registry has zero `SyntheticStub` | **MET** — at registration; see §2 |
| 3 | zero `CompatibilityStub` classes | **MET over 1351 classes, three live controls** — see §3 |
| 4 | every strict dispatch is bridge / service / intrinsic | **NOT MET**, and undercounted by construction |
| 5 | strict errors are actionable | **PARTIAL** — only the class ORIGIN is missing; see §5 |
| 6 | corpus shows no new divergence **and** CI census is blocking | **HALF** — corpus yes (today); blocking no |

## 1. Startup refusal — MET

```text
JAVA_HOME=/tmp/nojdk cratonvm --jdk-only -cp /tmp T
  --jdk-only requires a real JDK runtime image: real class bytes are
  authoritative under this policy, so there is nothing to fall back to. Point
  the launcher at an installation with --java-home <PATH>, or drop --jdk-only
  to run with the default compatibility behaviour.
  Searched, in order:
    --java-home = <not passed>
    CRATONVM_JAVA_HOME = <unset>
```

`-version` still prints, and says so honestly: `JDK class library root: NONE
FOUND — a program launch in this mode will fail`. That is the right split.

**One wart:** the launch failure exits **134** (`SIGABRT`), not a chosen status
code. The message is exemplary and the exit code is a crash. Worth a follow-up;
it is not a criterion failure.

## 2. Zero `SyntheticStub` in the final registry — MET, and "met" is weaker than it sounds

```text
                        natives   bridge   intrinsic   synthetic-stub
  --real-jdk  (compatible)  12876    10517         658             1701
  --jdk-only  (strict)      11172    10514         658                0
```

Criterion 2 asks about the FINAL registry and the final registry is clean. This
is registration-level and complete — not limited by what the probe loaded — so
unlike §3 it carries no reach caveat.

`stub_ratchet.rs` says the same thing about its own passing test and is right to:
*"passes today for a weak reason: `register()` refuses the stubs at the door …
Refused is not retired."* Quantified:

* **1584 distinct stub triples** exist in `native-builtins/src/` and run under
  `--real-jdk`.
* Of those, **7 are still answered by a native under `--jdk-only`** — the
  refused registration falls through to an older one, not to bytecode
  (5 `intrinsic`, 2 `bridge`): `ByteBuffer.allocate`, `allocateDirect`, and five
  `java.util.logging` accessors.
* **1577 have no native at all** in strict.

What happens to those 1577 is the part the census cannot fully answer:

| | triples |
|---|---:|
| class never loaded in the probe run — **census cannot say** | 869 |
| loaded, declared, has code → strict falls through to real bytecode | 399 |
| loaded, and the image does **not** declare the method | 308 |
| loaded, declared, no code, not `acc_native` | 1 |

The 308 are the interesting ones: a fabricated method with no counterpart in the
real image. Dropping them does not hand the call to the JDK, because the JDK has
nothing to hand it to.

## 3. Zero `CompatibilityStub` classes — MET, WITHIN A 22% REACH

```text
  --real-jdk   475 classes   13 compatibility-stub
  --jdk-only   484 classes    0 compatibility-stub
```

The 13 under compatible mode are `cratonvm/internal/Unmodifiable*` (11),
`java/util/Comparator$Native` and `java/util/Enumeration$Impl`.

**The caveat is the finding.** That census describes the classes the probe
workload actually loaded:

```text
  classes in the class-origin census        484
  distinct classes named by the registry   1297
  of those, loaded during the probe run     290   (22.4%)
```

### Widened, same day — and the narrow probe *was* hiding one

`scripts/jdk-only-census.sh` takes `PROBE_CP` and `PROBE_CLASS` to census a real
application instead of the built-in probe. Nothing in the tree had used them.
Re-run with `JdkOnlyCensusLoadProbe` as the workload — the corpus probe whose
purpose is loading broadly:

```text
                    classes in census   compatibility-stub
  narrow  --real-jdk        475                 13
  narrow  --jdk-only        484                  0
  wide    --real-jdk        765                 14      <- one MORE than narrow
  wide    --jdk-only        886                  0
```

**The compatible-mode column is the control, and it is live.** A strict zero
only means something if the instrument can see a compatibility class at all;
these runs see 13 and 14 of them. So `--jdk-only` showing zero over 886 classes
is a real measurement, not a silent instrument.

**And the wide run found a compatibility class the narrow one missed** —
`cratonvm/internal/StreamCollector`, absent from the 13. That is the reach
concern made concrete: the narrow probe's population was not merely smaller, it
was missing a member of the very set criterion 3 is about.

**Two different denominators, which are easy to conflate.** The class-origin
census grew 484 → 886 (+83%), but the share of *registry* classes actually
loaded moved only 290 → 354 of 1297 (22.4% → 27.3%). Criterion 3 is about class
origins, so 886 is the relevant figure; the 27.3% is the right number for any
claim about the native surface, and it is still low.

To widen further, point `PROBE_CP`/`PROBE_CLASS` at a real application — a
regression-suite corpus or H2 — rather than at a probe. The lever exists and
costs one census run.

### Three workloads, three live controls

One workload is one sample. Repeated across all three strict-corpus probes,
each with its own paired compatible-mode control:

```text
  probe                      policy   classes   compatibility
  JdkOnlyCensusLoadProbe     real       765          14
  JdkOnlyCensusLoadProbe     strict     886           0
  JdkOnlyBreadthProbe        real       891          14
  JdkOnlyBreadthProbe        strict     929           0
  JdkOnlyPlatformProbe       real       753          17
  JdkOnlyPlatformProbe       strict     843           0

  UNION of classes seen under --jdk-only:  1351
  every strict arm zero AND every control live:  true
```

**Every control is live** — 14, 14 and 17 compatibility classes respectively —
so none of the three zeros is a mute instrument. The check is mechanical: the
script fails the run if any compatible-mode arm reports zero compatibility
classes, because a strict zero next to a compatible zero measures nothing.

`JdkOnlyPlatformProbe` sees 17 where the others see 14, so which compatibility
classes appear is workload-dependent — one more reason a single probe's verdict
is worth less than three.

**Criterion 3 therefore reads: no `CompatibilityStub` class was observed under
`--jdk-only` across 1351 distinct classes, on three workloads whose controls
each saw between 14 and 17.** That is a real verdict. It is still not a
whole-image proof, and the honest next step is unchanged: point
`PROBE_CP`/`PROBE_CLASS` at a real application rather than at a probe.


## 4. Every strict dispatch is a bridge, a reviewed service, or an intrinsic — NOT MET

`report-strict.json`, 1799 violation rows:

```text
  synthetic-native-registered   1722    (registration attempts the policy refused)
  native-shadows-bytecode         77 -> native-won 55, bytecode-won 22
```

**55 natives win over real JDK bytecode under `--jdk-only`.** Each is a bridge
that shadows a method the image implements — `java/io/File.isDirectory()Z`,
`java/lang/Class.getName()`, `java/lang/Module.getDescriptor()` and 52 more.
Under §1.4's own definition those are exactly what strict mode exists to stop.

**And 55 is a floor, not a count.** The recorder skips `NativeKind::Intrinsic`
at all three sites, so intrinsic shadows cannot appear in this number at all —
measured at **398** on 2026-08-22
(`WORKER-3-NOTE-5-the-census-exempts-398-shadows-20260822.md`). Any quoted
figure here needs `+398 exempt` beside it, and re-kinding a row `Intrinsic`
removes it from the census without changing behaviour.

## 5. Actionable strict errors — PARTIAL, and narrower than this page first said

> **CORRECTED 2026-09-10, same day.** The first version of this section was
> written from `report-strict.json` alone and concluded that fallback
> instructions were missing. **They are not.** A strict run emits violations
> through `JdkOnlyViolation::render`, not through the JSON, and reading one
> artifact and generalising to "strict errors" is the whole error. Recorded
> rather than quietly replaced, because acting on the wrong version would have
> meant adding a remediation line that is already there and already tested.

There are two surfaces and the criterion is about the first one.

**The rendered error** — `types/src/error.rs::render`, what an operator sees:

```text
CratonVM --jdk-only: policy violation [native-shadows-bytecode]
  requested class:   ...
  requested member:  ...
  requested from:    ...
  initiating loader: ...
  reason:            ...
  JDK:
    feature version: 25
    java.home:       ...
    module:          ...
  Remediation:
    ...
    re-run with --real-jdk to restore the current compatibility behaviour
    capture the full machine-readable report with --jdk-only-report <FILE>
```

| criterion field | present? | where |
|---|---|---|
| class | yes | `requested class` |
| method, descriptor | yes | `requested member` |
| attempted native kind | yes | inside `reason` (`bridge native shadows bytecode of …`) |
| JDK version | yes | `JDK: feature version`, plus `java.home` and `module` |
| fallback instructions | **yes** | `REMEDIATION_FALLBACK`, pinned as the penultimate line |
| class origin | **no** | — |

The fallback is not incidental: `remediation_ends_with_the_two_fixed_lines`
iterates `all_variants()` and asserts the fallback is the penultimate line and
the capture hint the last, for every variant.

**The JSON row** — `to_json`, the machine-readable report: carries `kind`,
`summary`, `class`, `method`, `descriptor`, and per-variant fields
(`native_kind`, `outcome`, `registered_by`, …). JDK version is at the report top
level as `jdk_feature` rather than per row, and the fallback string is not
repeated 1799 times, which is the right call for a constant.

**So the one unambiguous gap, on both surfaces, is the class ORIGIN.**

And it may not be satisfiable uniformly. Six of the fourteen construction sites
build `SyntheticNativeRegistered`, which fires at REGISTRATION — before any
class is loaded, so there is no class origin in existence to report. The field
is meaningful for the class-facing variants (`CompatibilityClassRequested`,
`NativeShadowsBytecode`) and vacuous for the registration-time ones. A change
that adds `origin: null` to the latter to tick the box would make the criterion
read as met without telling anyone anything.

## 6. Corpus green **and** a blocking CI census — HALF

The corpus half is met as of 2026-09-10: `21-linux` and `25-linux` carry **zero**
rows and the ratchet still fires (`50b9c2482`). The Windows keys are stale rather
than live and cannot be re-minted from this build machine.

The blocking half is not met: `.github/workflows/ci.yml`'s `jdk-only` job is
still `continue-on-error: true`.

## 7. Why promoting that job today would be wrong

`ci.yml` invites it. Its comment gives two reasons for staying advisory and says
of the second: *"Promote this job by deleting `continue-on-error` at that point,
together with un-ignoring `strict_mode_refuses_nothing`"* — where "that point"
is the corpus being deterministic, which happened today.

`native-builtins/tests/stub_ratchet.rs` says the opposite about the same test.
Un-ignoring `strict_mode_refuses_nothing` requires **all three** of: reclassify
or delete the 549 `SyntheticStub` registrations, drive
`BASELINE_SYNTHETIC_STUBS` to 0 in the same change, *then* un-ignore and promote
CI. Corpus determinism is not on its list at all.

So one file makes corpus determinism sufficient and the other makes it
irrelevant. Taking `ci.yml` at its word would promote a gate whose own strong
assertion is still `#[ignore]`d, over work the design calls a separate wave with
subsystem-per-PR discipline. **The stub reclassification governs, and it is not
done.**

## 8. Three cited counts, none of which matches the dump

| source | says | of what |
|---|---:|---|
| `ci.yml` comment | 157 | "synthetic-stub natives still registered on the default path" |
| `stub_ratchet.rs` doc comments | 549 | "the real boot-path registrar … has 549 stubs in it" |
| `BASELINE_SYNTHETIC_STUBS` | 1646 | distinct stubs, management arm |
| **this run** | **1701 rows / 1584 triples** | `kind == synthetic-stub` in `registry-real.json` |

The ratchet constant is live and is re-frozen when it moves, so 1646 is a
different denominator rather than a stale one. **157 and 549 are prose**, and
prose does not get re-frozen. Anyone reasoning about how much work remains
should take it from a dump.

## Reproducing

```
CV=target/release/cratonvm JAVA_HOME=<jdk25> OUT=target/audit BLOCKERS=off \
  sh scripts/jdk-only-census.sh
python3 - target/audit <<'EOF'
import json,collections
for pol in ("real","strict"):
    j=json.load(open("target/audit/registry-%s.json"%pol))
    print(pol, j["counts"])
EOF
```

`registry-*.json`'s rows live under the key `natives`; `kind` is the only kind
field (`kind_stated` and `kind_chosen` are booleans, not names — reading them as
kinds silently reports zero of everything).
