# TestJspWriterImpl — JspWriter.print(Object) output mismatch (bug54241b)

**Status:** FIXED, merged to `dev` (`c9b646e8c`). **Severity:** low.
**HotSpot:** PASS (fresh-verified).

## Summary

`org.apache.jasper.runtime.TestJspWriterImpl.bug54241b` failed:
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
diverged from what `String.valueOf(obj)` should produce for whatever
object this test passes.

Found via a fresh Windows full-suite rerun (dev commit `080e79256`,
2026-07-12, real JDK, JIT on). Verified via a fresh same-session HotSpot
run: PASSES cleanly.

## Root cause

`bug54241b.jsp` defines an anonymous class whose `toString()` explicitly
`return null;`, then does `<%= bug54241 %>`, which the JSP compiler emits
as `out.print(bug54241)` → `JspWriterImpl.print(Object obj)` →
`write(String.valueOf(obj))`. Per the JDK contract,
`String.valueOf(Object obj)` for a non-null `obj` returns exactly
`obj.toString()` — including a legitimate `null` if `toString()` itself
returns `null`. `java.io.Writer`'s default `write(String str)` then does
`write(str, 0, str.length())`, which legitimately throws
`NullPointerException` on that real `null` — HotSpot's actual (if
surprising) behavior, producing a 500 response, which is exactly what
`bug54241b` asserts (`SC_INTERNAL_SERVER_ERROR`).

CratonVM's native override for `String.valueOf(Ljava/lang/Object;)`
(`native_string_value_of_object` in `native-builtins/src/lang_string.rs`)
delegated to a shared helper, `invoke_to_string`, that calls
`obj.toString()` via virtual dispatch and only had a match arm for
`Ok(Some(Value::Object(Some(str_ref))))` (a real String result) —
`Ok(Some(Value::Object(None)))` (toString() legitimately returning null)
fell into the same catch-all fallback as a genuine dispatch failure,
which fabricates a `ClassName@hash` string. So `String.valueOf(obj)`
returned a bogus non-null string instead of propagating the real null,
`write(String)` never NPE'd, and the JSP rendered 200 with visible content
instead of 500.

## Fix

Split `invoke_to_string` into a thin wrapper plus `invoke_to_string_opt`,
which distinguishes "toString() legitimately returned null" (`Ok(None)`)
from "dispatch failed" (still falls to the `ClassName@hash` fallback).
`native_string_value_of_object` now propagates that `None` as an actual
null `Value::Object(None)` instead of coercing it to the text `"null"`,
matching the JDK's null-preserving contract. `invoke_to_string`'s other 14
call sites (`StringBuilder.append(Object)`, string concatenation, etc.)
keep coercing to `"null"` text, matching
`AbstractStringBuilder.append(String)`'s own null handling and the
already-correct sibling `value_to_string_deep` in
`vm/src/runtime/invokedynamic.rs`.

## Verification

`TestJspWriterImpl.bug54241a`/`bug54241b` both PASS via the local
`apps/tomcat-suite-runner` harness (`bug54241b` now gets a real
NPE-driven 500, not a bogus 200). Full `native-builtins` crate test suite:
2974 passed, 12 pre-existing failures unrelated to string/toString
handling (`jboss_msc`, `key_factory`, `jspecify` annotations,
`ByteBuffer`, security policy, xerces XML — none touch `lang_string.rs`),
confirmed identical against unmodified `dev` too.

Fixed+merged via `fix/jspwriter-print-object-20260713-local`, commit
`c9b646e8c`.

## Reproduction (historical)

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName jspwriterimpl `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.jasper.runtime.TestJspWriterImpl
```
