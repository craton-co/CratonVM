# Bouncy Castle crypto-prng — abnormal exit 127 after all tests Okay (BC-PRNG-1)

## Status
**OPEN** — harness anomaly on `target/release/cratonvm.exe` (2026-06-05 apps suite).

## Severity
**LOW** (correctness) / **MEDIUM** (harness) — all regression subtests pass; process exit code is wrong.

## App / suite
- **Tree:** `apps/_test-suites/bc-java/`
- **Main:** `org.bouncycastle.crypto.prng.test.RegressionTest`
- **Classpath:** `core/build/classes/java/{main,test}` + resources
- **Heap:** 1g · **Harness:** `test-infra/run-all-apps-suites.sh`
- **Log:** `test-infra/suite-results/apps-all-20260605-170945/bc-crypto-prng-cratonvm.log`

## Symptom

Log shows **complete success** for every subtest:

```
CTRDRBGTest: Okay
DualECDRBG: Okay
HashDRBG: Okay
HMacDRBG: Okay
SP800RandomTest: Okay
```

Yet the shell reports:

- **rc:** 127 (not 0)
- **wall:** 62.6 s
- **Harness state:** FAIL (strict rc check) → HotSpot comparison skipped

No exception, SEGV, or `System.exit` line appears after the last `: Okay`.

## HotSpot behavior

Same `RegressionTest` on same classpath: all `: Okay`, **rc=0**, ~0.5–1 s.

## CratonVM behavior — asn1 contrast

`org.bouncycastle.asn1.test.RegressionTest` on the same run: **rc=0**, PASS, 43.2 s. Only crypto-prng shows rc=127 after green output.

## Root cause (suspected)

Unknown. Hypotheses:

1. **Process termination after main returns** — CratonVM or wrapper exits 127 without propagating main's 0 (shutdown hook / finalizer / native teardown).
2. **`timeout` or parent signal** — 62 s wall is long for prng suite; possible watchdog or external kill with misleading rc (GNU `timeout` uses 124, not 127).
3. **Hidden `System.exit` in BC teardown** — not captured in log buffer before process death.

127 on Windows often means **command not found** when the shell mis-resolves the binary — less likely here since output is complete.

## Workaround for harness

Treat log content as authoritative when all lines match `: Okay` and no error signatures — mark PASS despite rc=127 until exit path is fixed (`run-all-apps-suites.sh` has partial logic for this).

## Reproduce

```bash
BC="apps/_test-suites/bc-java"
CP="$BC/core/build/classes/java/main;$BC/core/build/classes/java/test;…resources…"
cratonvm.exe --java-home "<jdk-25>" --Xmx 1g \
  -cp "$CP" org.bouncycastle.crypto.prng.test.RegressionTest
echo "rc=$?"
```

Compare wall and rc to HotSpot on identical `-cp`.

## Fix direction

1. Log CratonVM process exit reason (main return vs `System.exit` vs signal).
2. Run under debugger / `--nojit` to see if JIT teardown differs.
3. Diff last frames after final `: Okay` between HotSpot and CratonVM.

## Related

- BC asn1 same run: PASS rc=0 (`bc-asn1-regression`)
- [apps/CRATONVM_CRASHES.md](../../apps/CRATONVM_CRASHES.md)
- Cross-VM BC harness: `test-infra/bc-suite-3way.sh`
