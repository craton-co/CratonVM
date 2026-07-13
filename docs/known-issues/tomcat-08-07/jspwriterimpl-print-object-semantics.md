# TestJspWriterImpl — JspWriter.print(Object) output mismatch (bug54241b)

**Status:** OPEN. **Severity:** low. **HotSpot:** PASS (fresh-verified).

## Summary

`org.apache.jasper.runtime.TestJspWriterImpl.bug54241b` fails:
```
1) bug54241b(org.apache.jasper.runtime.TestJspWriterImpl)
java.lang.AssertionError:

<html>
  <body>

    <!-- JspWriter.print(Object obj) is defined to print String.valueOf(obj)
    ...
```
(The assertion message is a full expected-vs-actual HTML body diff, cut
off in the captured log — see reproduction below for the full text.) The
comment embedded in the test's expected output states the contract
directly: `JspWriter.print(Object obj)` is specified to print
`String.valueOf(obj)`. This test (`bug54241b`, a regression test for a
historical Jasper bug) is checking that contract holds for some specific
object/value combination — CratonVM's `JspWriter.print(Object)` output
diverges from what `String.valueOf(obj)` should produce for whatever
object this test passes (likely `null`, or an object with a specific
`toString()`/boxing edge case, given `bug54241b`'s sibling `bug54241a` in
the same class passed in an earlier investigation this session).

Found via a fresh Windows full-suite rerun (dev commit `080e79256`,
2026-07-12, real JDK, JIT on). Verified via a fresh same-session HotSpot
run: PASSES cleanly.

## Reproduction

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName jspwriterimpl `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.jasper.runtime.TestJspWriterImpl
```

## Recommendation

Read `TestJspWriterImpl.bug54241b`'s source and the full HTML diff in its
captured log to see exactly which value's `String.valueOf()` rendering
differs. Given the `String.valueOf(Object)` contract is explicit here,
trace CratonVM's `JspWriter.print(Object)` implementation (whether it's
the real Tomcat bytecode calling through to a real-mode `String.valueOf`,
or a native shim) for the specific object type this test exercises — likely
a `null`-handling edge case (`String.valueOf(null)` should print the
literal string `"null"`) or a boxed-primitive `toString()` formatting
difference.
