# The regression suite's six mode-specific failures are the harness, not the mode

**Status: MEASURED 2026-09-02** on `azure-host-2` (`azureuser@20.80.105.49`),
binary `/data/l7dod-target/debug/cratonvm` built from this tree, oracle Temurin
25 on the same host. **VERIFIED AGAINST A BINARY 2026-09-02.**

**Lane** L7. **Subject** `regression-suite/run.sh`, the `--jdk-only` arm.

---

## 1. What it looked like

Running H9-1 §7b's three arms produced what appears to be the finding this whole
lane exists to detect — vectors that fail under `--jdk-only` and pass without it:

```text
arm 1  CRATONVM_ARGS=--jdk-only   119/125   RJitStackTraceLines RJitMultiArrayClass
                                            RMapGcStress RJdkIntrinsics2
                                            RJdkIntrinsics3 RJdkFailure
arm 2  SUITE=all                  119/125   RJitStackTraceLines RMapGcStress
                                            RJdkIntrinsics3 RBufferPoolCount
                                            RJdkJmx RJdkEnvMap
arm 3  SUITE=core                  81/85    RJitStackTraceLines RMapGcStress
                                            RJdkIntrinsics3 RBufferPoolCount
```

**And arms 1 and 2 are a clean A/B**, which is what made it credible. The
scheduler builds `core` + `JDK_ONLY` and `all` from the same two lists:

```bash
core)  SUITE_SET="$CORE_CLASSES${JDK_ONLY:+ $JDKONLY_CLASSES}" ;;
all)   SUITE_SET="$CORE_CLASSES $JDKONLY_CLASSES" ;;
```

so both scheduled the identical 125 vectors and differed only in the flag. Three
vectors fail strict and pass compatible; three do the reverse.

## 2. What it is

**All six pass when run alone, in BOTH modes.**

```text
ONLY="RJitMultiArrayClass RJdkIntrinsics2 RJdkFailure"     (the "strict-only" three)
  A strict  3 passed, 0 failed        B compat  3 passed, 0 failed
  B compat  3 passed, 0 failed        A strict  3 passed, 0 failed

ONLY="RBufferPoolCount RJdkJmx RJdkEnvMap"                 (the "compat-only" three)
  A strict  3 passed, 0 failed        B compat  3 passed, 0 failed
  B compat  3 passed, 0 failed        A strict  3 passed, 0 failed
```

Run **ABBA-interleaved** — strict, compat, compat, strict — so a monotone load
drift across the sequence cannot produce a difference that looks like a mode
effect. Every row carries the binary's mtime and all eight agree, so this is one
binary throughout.

So the six are not a property of `--jdk-only`, and not of compatible mode
either. They are produced by the FULL-SUITE RUN: load, ordering, or state shared
between vectors. **A concurrent or full-suite run FINDS candidates; only running
one alone CONFIRMS it.**

## 3. What survives

Three vectors fail in both arms — `RJitStackTraceLines`, `RMapGcStress`,
`RJdkIntrinsics3`. Those are mode-independent and were not re-run alone here;
`RMapGcStress` failing is `H9-1` §8's own standing prediction, so at least one of
the three is expected.

**The lane's headline is unchanged and now has a second, independent
confirmation:** `--jdk-only` introduces no failure that compatible mode does not
also have. `P4A-a-corpus-under-jdk-only-for-the-first-time-20260829.md` reached
that on the H2/Spring/Tomcat corpus, 218 vectors. This reaches it on the `RJdk*`
regression suite, 125 vectors, by a different route — and the one apparent
counter-example dissolved on contact.

## 4. Two ways this nearly went wrong

**The first: publishing arm 1 vs arm 2 as a result.** It is a genuine A/B on an
identical schedule, one flag apart. Everything about it reads as a finding. What
it lacks is isolation, and this host has manufactured three defect-shaped
results for this lane already — an `ENOSPC` from a full `/`, an OOM from three
concurrent shards, and a global OOM kill. A load swing between two sequential
125-vector runs looks exactly like a mode difference.

**The second, worse: the confirmation run silently produced nothing.** The first
ABBA attempt piped each run through `grep "REGRESSION SUITE:"`. Mid-session the
CLI binary vanished from `debug/` — evicted by a sibling `cargo test -p <crate>`
sharing `CARGO_TARGET_DIR`, not by another lane, none of which use this target
dir. `run.sh` then printed

```text
ERROR: CratonVM binary not found: /data/l7dod-target/debug/cratonvm
```

and no summary, so the grep returned empty and the script printed four tidy rows:

```text
A strict  ::
B compat  ::
```

**A run that could not start renders identically to a run with nothing to
report.** Skimmed as "no failures", it would have refuted the strict-only
candidates on the strength of a missing binary — the right conclusion reached
from no evidence at all.

The rerun asserts the binary is executable before each arm, prints `NO SUMMARY`
with the error line whenever a summary is absent, and stamps the binary's mtime
on every row. Any harness that reports by grepping for a success line needs the
same three things, because the failure mode is silent by construction.

## Reproduce

```bash
source /data/toolchain/env.sh
export CV=<tree>/target/debug/cratonvm JDK=$JAVA_HOME
# the A/B that looks like a finding
TIMEOUT=420 CRATONVM_ARGS=--jdk-only bash regression-suite/run.sh
TIMEOUT=420 SUITE=all                bash regression-suite/run.sh
# the check that dissolves it -- alone, and ABBA, and assert the binary each time
for arm in --jdk-only "" "" --jdk-only; do
  [ -x "$CV" ] || { echo "ABORT: binary missing"; break; }
  TIMEOUT=420 ONLY="RJitMultiArrayClass RJdkIntrinsics2 RJdkFailure" \
    CRATONVM_ARGS="$arm" bash regression-suite/run.sh | grep "REGRESSION SUITE:"
done
```
