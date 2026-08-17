# G62-1 — `--jdk-only` is now strictly better than Compatible mode

**Status:** MEASURED. **Provenance:** two full suite runs per arm, on two
binaries, one command apiece. Oracle HotSpot 25.0.3+9-LTS. No claim here is
inferred from a single arm.

---

## 0. The measurement

| arm | `e7e840264` (before this session's last six fixes) | `89e2c56f1` (now) |
|---|---|---|
| `CRATONVM_ARGS="--jdk-only"` | 97 of 99 | **99 of 99** |
| `SUITE=all` (Compatible mode) | 92 of 99 | **94 of 99** |

Both arms improved by exactly the vectors that were fixed, and **no vector
regressed in either arm**. The Compatible-mode failure lists are nested:

```text
e7e840264   RImmutableFactoryTypes RJdkBridge1 RSslLiveSession RJdkProxyIface
            RJdkFunctionCombinators RJdkEnumerations RServiceLoaderDoubleSource
89e2c56f1   RImmutableFactoryTypes                 RJdkProxyIface
            RJdkFunctionCombinators RJdkEnumerations RServiceLoaderDoubleSource
```

Same five, minus the two that closed. That nesting is the attribution: the
five survivors are pre-existing and none of this session's changes touched
them.

## 1. The finding

**Those five vectors PASS under `--jdk-only` and FAIL in Compatible mode, on
the same binary.**

```text
RImmutableFactoryTypes    --jdk-only PASS   Compatible FAIL
RJdkProxyIface            --jdk-only PASS   Compatible FAIL
RJdkFunctionCombinators   --jdk-only PASS   Compatible FAIL
RJdkEnumerations          --jdk-only PASS   Compatible FAIL
RServiceLoaderDoubleSource --jdk-only PASS  Compatible FAIL
```

The only difference between the two arms is **which bodies run**. Under
`--jdk-only` the synthetic-stub registrations are refused and the real JDK
class bytes execute; in Compatible mode the stubs register and dispatch. Five
vectors get the right answer from the JDK and the wrong answer from us.

This is the first time in this branch's history that the strict arm is
**ahead** of the permissive one. The mode was built as a conformance
constraint — a thing you turn on to find out what is fake — and it is now the
configuration that gets more right.

## 2. Why this is the strongest available evidence for stub removal

`G60-1` measured the population from the mode's own census: 1,341 synthetic
registrations refused, 0 synthetic-stub invocations, and 81 natives that win
over real bytecode. What that census could not say is whether any of it
*matters* — a refused registration whose real bytecode nobody exercises is a
count, not a defect.

These five vectors are the answer. They are the same code, on the same
binary, differing only in whether the stubs are allowed to run, and the stubs
lose. **The census says how much fakery exists; this says it is wrong where it
is exercised.**

It also means the two arms are no longer "strict" and "lenient". Compatible
mode is now a distinct, measurably worse configuration, and any argument for
keeping a synthetic stub has to explain why these five rows do not generalise.

## 3. What this record does NOT claim

* **Not that the five are one defect.** Nobody has looked at them. They may be
  five unrelated stubs or one shared registrar; `--only=<family>` and
  `--dump-native-registry` will say, and per `G61-1` §1 that mapping costs
  about a minute per vector and should precede any fix.
* **Not that Compatible mode is unnecessary.** It is what `--real-jdk` and the
  ratchet baselines are scored against, and `retired_shadow.rs` exists
  precisely to keep a retired stub working there. This record measures a gap;
  it does not propose deleting the arm.
* **Not that 99 of 99 means the mode is finished.** The suite is 99 vectors.
  `G60-1` §4 states the standing bound: one vector's census is a floor, and
  the whole suite is still a small program compared with an application.

## 4. NOMINATIONS

**N1 — triage the five.** They are the highest-value stub-removal targets in
the tree, because each one already has an oracle-differential vector proving
the real bytecode is right. That is a far stronger starting position than the
24 pure-Java triples in `G60-1` §2, which have no failing row behind them.

**N2 — run both arms in CI, not just one.** This gap was invisible for the
whole session because only the `--jdk-only` arm was being measured. Two arms
cost two commands and would have surfaced it on day one.

**N3 — `G60-1` N3 re-raised and now sharper.** Run `--jdk-only-report` against
a real application. `APP-READINESS-20260812.md` has the Spring and Tomcat
classpaths, and its harness caveats were cleared on 2026-08-17, so the run
costs one command. With the suite at 99 of 99, an application is the only
place a new defect can now come from.
