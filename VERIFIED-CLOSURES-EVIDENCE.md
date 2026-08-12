# Measured on the current dev binary (ba65f1a19, built 2026-08-12 00:21)

Binary: `C:/craton/CratonVM/target/release/cratonvm.exe`
Both runtime modes run the SAME class files from `regression-suite/build`.

| inherited record | its vector | `--jdk-only` | `--real-jdk` |
|---|---|---|---|
| L8-securerandom-provider | RJdkSecurity | PASS (61 checks) | PASS (61 checks) |
| W4-3-security-getalgorithms-short-list | RJdkSecurity | PASS (61 checks) | PASS (61 checks) |
| L16-classnotfound-vs-noclassdeffound-shapes | RJdkFailure | PASS (43 checks) | PASS (43 checks) |
| W5-1-loadlibrary-allowlist-too-wide | RJdkJni | PASS (35 checks) | PASS (35 checks) |
| W6-2-module-serviceloader-provider-factory | RJdkModule | PASS (44 checks) | PASS (44 checks) |

RJdkModule needs `--module-path regression-suite/build-modules --add-modules cratonvm.jdkonly.svc`
(run.sh supplies these via `class_args`); without them it fails on a harness
error, not a VM defect.

W6-2 recorded its vector walking 1 -> 4 -> 14 -> 20 -> 26 -> ~30 of 44.
It is now 44/44.

## The caveat that must not be dropped

Every one of these five records said "fix written / FIXED in source, **not yet
verified against a binary**". The measurement above is that missing
verification, and it closes the **headline** defect in each.

It does **not** close the residual sections. At least these are separate and
still need reading:

- W4-3: "two kept residuals re-verified as live" + "six out-of-file patches
  recorded and NOT applied".
- L8: "the kept residual is live, but the reason given for keeping it is wrong
  in a way that matters, and a second shadowed registration beside it is worse
  than the one that was named".
- L16: "fix PARTIAL -- the two one-line guard patches that consume the helper
  are in native-builtins/, which that lane did not own, recorded verbatim".
  RJdkFailure passing means the vector no longer sees it; whether the guards
  landed is a separate question.
- W5-1: headline fixed; the loader-scoped `loadedLibraryNames` residual landed
  in **strict mode only**, unbuilt at the time.
- W6-6 (not run above) depends on W5-1's mechanism.

So: retire the headline, keep or re-file the residual. Do not retire a whole
record because its vector went green.
