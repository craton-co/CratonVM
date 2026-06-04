# Bug: Bouncy Castle suite — separate real fails from harness artifacts

The `test-infra/bc-suite-3way.sh` / `run-vm-comparison.sh` BC results mix genuine VM
failures with harness artifacts. Clean this up so the BC pass/fail picture is trustworthy.
Independent of the other `continue_prompt_*` bugs. (math-ec is excluded — it's blocked by
the `org/bouncycastle/` JIT ban, handled by the separate BC-EC codegen effort.)

## Items
- `crypto-prng-regression`: now **PASSES** on CratonVM (rc=0, ~59s). Confirm + record.
- `util-encoders`: harness shows FAIL, but it **passes when run cleanly** ("OK (15 tests)").
  Root cause is the harness classpath: under MSYS/Git-bash the `;`-separated `-cp` is
  mangled so `junit.textui.TestRunner` isn't found (fails identically on HotSpot — so NOT a
  VM bug). **Fix the harness** (write the classpath to a Java `@argfile`, or invoke the
  Windows `java.exe` with a properly-quoted native cp), then re-measure.
- `pqc-crypto-regression`: was TIMEOUT, now **completes-then-fails**. Capture the actual
  failure on a clean run — is it a real BC assertion or a wrong CratonVM intrinsic? (Run
  `org.bouncycastle.pqc.crypto.test.RegressionTest` directly and read the first failing
  line.)
- `crypto-regression`: OOMs at `-Xmx1g` on HotSpot too — not a fair test at 1g. Bump heap
  (e.g. 4g) or mark inconclusive.

## Repro
```
BC=apps/_test-suites/bc-java
CP="$BC/core/build/classes/java/main;$BC/core/build/classes/java/test;$BC/core/build/resources/main;$BC/core/build/resources/test"
JUNIT="$TEMP/junit-3.8.2.jar"
CV=target/release/cratonvm.exe; JDK="C:/Program Files/Java/jdk-25"
# SimpleTest-style (no junit): prints "<Name>: Okay"
$CV --java-home "$JDK" -cp "$CP" org.bouncycastle.crypto.prng.test.RegressionTest
$CV --java-home "$JDK" -cp "$CP" org.bouncycastle.pqc.crypto.test.RegressionTest
# JUnit-textui (needs junit jar; mind the MSYS cp mangling): "OK (n tests)"
$CV --java-home "$JDK" -cp "$CP;$JUNIT" junit.textui.TestRunner org.bouncycastle.util.encoders.test.AllTests
```
Compare each against `C:/Program Files/Java/jdk-25/bin/java.exe` with the same cp.
Memory: `reference_cross_vm_comparison_harness`.
