# `JarVisitorTest` "CRASH rc=0/ms=0" — confirmed NOT a CratonVM bug

| | |
|---|---|
| **Status** | ✅ CONFIRMED NOT A BUG — harness classpath-layout limitation, reproduces identically on real HotSpot. |
| **Discovered / closed** | 2026-07-16, re-verification of `docs/known-issues/hibernate/hib-misc-residuals-20260716.md` `JarVisitorTest` entry. |
| **Class** | `org.hibernate.orm.test.bootstrap.scanning.JarVisitorTest` (and, structurally, every sibling that extends `PackagingTestCase`, e.g. `ScannerTest`). |

## Original report

The 2026-07-16 full-suite rerun (local Windows host, `dev@2f02e939d`) recorded
`JarVisitorTest` as `process-died rc=0 ms=0` — a shape previously associated
with harness/logging artifacts produced during a disk-space-exhaustion
incident on that same host. The doc correctly flagged this as *suspected but
unverified*.

## Re-verification performed

Ran solo against the frozen, already-verified `dev@dcb24161` baseline binary
(`/data/data/frozen-hib-misc-basecheck-dcb24161-20260716` on the Azure Linux
probe host), 5x `--nojit` + 5x JIT-on, each wrapped in `timeout 120`:

- **10/10 runs**: clean process exit, `rc=0`, elapsed **887ms–1913ms** (never
  0ms), each producing a well-formed result line:
  ```
  @@FAIL org.hibernate.orm.test.bootstrap.scanning.JarVisitorTest :: java.lang.AssertionError: Unable to setup packaging test : could not interpret url
  @@RESULT 0 org.hibernate.orm.test.bootstrap.scanning.JarVisitorTest found=9 started=0 ok=0 failed=0 aborted=0 skipped=0 ms=887..1913
  @@DONE
  ```
- No crash, no hang, no non-deterministic behavior across any of the 10 runs
  (both JIT configurations). This is a **fully deterministic, reproducible
  FAIL**, not the `rc=0 ms=0` artifact shape from the original report.

This corroborates the doc's harness-artifact hypothesis for the *specific*
`rc=0/ms=0` row (a genuinely healthy run never produces that shape — it
always completes with real elapsed time and a real message), but it does
**not** corroborate "passes cleanly" — the class does not pass; it fails
every time, for a reason entirely unrelated to CratonVM's correctness (see
root cause below).

## Root cause

`PackagingTestCase` (the shared base class for `JarVisitorTest`, `ScannerTest`,
and other `bootstrap.scanning`/`jpa.pack.*` tests) has a **static
initializer** that locates its own class file via
`ClassLoader.getResource(...)`, then does:

```java
URL myUrl = originalClassLoader.getResource(
        PackagingTestCase.class.getName().replace('.', '/') + ".class");
...
if (myUrl.getFile().contains("target")) { index = ...; }        // Gradle/Maven convention
else if (myUrl.getFile().contains("bin")) { index = ...; }       // some IDEs
else if (myUrl.getFile().contains("out/test")) { index = ...; }  // IntelliJ
if (index < 0) {
    fail("Unable to setup packaging test : could not interpret url");
}
```

This is a heuristic to find the build-output root so the test can create a
sibling `target/bundles` / `target/packages` directory for ShrinkWrap to
build `.jar`/`.par`/`.war` fixtures into. It assumes the test-classes
directory path contains one of `target`, `bin`, or `out/test`.

The `hib-suite-runner` harness's classpath places compiled test classes at:

```
/data/data/apps/hibernate-orm-harness/hib-libs/test-classes
```

— which contains **none** of those substrings. The static initializer's
`fail(...)` (JUnit4 `Assert.fail`, throws `AssertionError`) therefore always
fires, unconditionally, the first time any `PackagingTestCase` subclass is
loaded in a given JVM process, regardless of which JVM is running it.

### Verified VM-independent (real HotSpot repro)

Wrote a minimal probe (`CheckUrl.java`) and ran it with the **real JDK 25**
(`/home/victor/jdk25/bin/java`), using the exact same classpath entry the
harness uses:

```
URL: file:/data/data/apps/hibernate-orm-harness/hib-libs/test-classes/org/hibernate/orm/test/bootstrap/scanning/PackagingTestCase.class
contains target: false
contains bin: false
contains out/test: false
```

Identical to what CratonVM sees. **This proves the failure is a harness
classpath-layout artifact, not a CratonVM defect** — under this exact
harness classpath layout, real HotSpot would fail this same assertion in the
same way. There is no CratonVM code change that could make this class pass;
the only fix would be a harness-config change (renaming/symlinking the
test-classes output directory to contain `target`, `bin`, or `out/test`),
which is outside the CratonVM codebase and out of scope here.

### Bonus check: repeated-failed-class-init handling is correct (no CratonVM bug found)

Since `PackagingTestCase`'s static init throws on first load, JLS §12.4.2
says any *subsequent* attempt to initialize that class in the same JVM
process must throw `NoClassDefFoundError` (not re-run the initializer or
re-throw the original error type). `ScannerTest` also extends
`PackagingTestCase`, so this is directly exercisable: ran a list of
`JarVisitorTest` then `ScannerTest` in one process (mirroring how the
original 4-shard full run drives many classes per process). Result:

```
@@FAIL org.hibernate.orm.test.bootstrap.scanning.JarVisitorTest :: java.lang.AssertionError: Unable to setup packaging test : could not interpret url
@@RESULT 0 ... JarVisitorTest ... ms=1910
@@FAIL org.hibernate.orm.test.bootstrap.scanning.ScannerTest :: java.lang.NoClassDefFoundError: org/hibernate/orm/test/bootstrap/scanning/PackagingTestCase
@@RESULT 1 ... ScannerTest ... ms=369
```

CratonVM correctly produces `NoClassDefFoundError` on the second class-load
attempt and fails fast (369ms) — no hang, no misattribution. This rules out
a "failed static init state mishandled as a hang" explanation for the
*separate* `ScannerTest` 120s-timeout entry tracked in
[hib-120s-junit-timeout-cluster-20260716.md](hib-120s-junit-timeout-cluster-20260716.md)
— that timeout has a different, still-open cause (systemic throughput gap,
per that doc), not this classpath issue.

## Live disk-pressure corroboration (informational, not proof of the original incident)

At the time of this re-verification, `df -h /` on the same probe host showed
root at **100% full (41M free)** — consistent with disk-pressure-driven
harness artifacts being a real, recurring phenomenon on hosts running this
suite. This is pattern-consistent evidence only; it does not directly prove
the original Windows-host run was disk-constrained at the exact moment
`JarVisitorTest` ran, but it does support that such artifacts are plausible
and not far-fetched on this class of infrastructure.

## Conclusion

- The `rc=0/ms=0` "CRASH" shape from the original report is not reproducible
  and was very likely a harness/logging artifact, as the original doc
  suspected.
- However, `JarVisitorTest` genuinely does not (and cannot) pass under the
  current `hib-suite-runner` harness classpath layout — this is a **harness
  configuration limitation that also affects real HotSpot**, not a CratonVM
  defect. No CratonVM code fix applies.
- No worktree/fix/merge was performed — there is nothing to fix in the VM.
