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
| 3 | zero `CompatibilityStub` classes | **MET WITHIN REACH** — the census saw 22% of the classes |
| 4 | every strict dispatch is bridge / service / intrinsic | **NOT MET**, and undercounted by construction |
| 5 | strict errors are actionable | **PARTIAL** — the startup error is; the violation rows are not |
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

So criterion 3 is adjudicated over roughly a fifth of the surface, and the
verdict should be read as *no compatibility class appeared in the classes this
workload touched* — not as a whole-image property. Widening it is a matter of
`PROBE_CP` / `PROBE_CLASS` (the census script takes both) rather than of new
machinery, and until someone does, a class-origin claim from this artifact
carries "over 484 classes" beside it the way a shadow count carries
"+398 exempt".

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

## 5. Actionable strict errors — PARTIAL

The criterion asks for class, method, descriptor, origin, attempted native kind,
JDK version and fallback instructions.

| field | on a violation row | where it is |
|---|---|---|
| class, method, descriptor | yes | row |
| attempted native kind | yes | `native_kind` |
| JDK version | no | `jdk_feature`, report top level |
| class origin | **no** | not on the row |
| fallback instructions | **no** on the row | present in the *startup* error (§1) |

A row reads `bridge native shadows bytecode of java/io/File.isDirectory()Z
[bytecode-won]`. That is a good diagnostic and it is not what §11 specifies.
The gap is the per-violation rows, not the launcher.

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
