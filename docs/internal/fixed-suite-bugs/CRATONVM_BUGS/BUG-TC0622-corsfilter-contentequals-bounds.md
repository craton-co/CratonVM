# Bug TC0622 — `String.contentEquals(StringBuilder)` SIOOBE (synthetic StringBuilder layout read as real `AbstractStringBuilder` by `nonSyncContentEquals`)

> **One-line root cause:** `String.contentEquals(CharSequence)` against a
> `StringBuilder` runs the real JDK `String.nonSyncContentEquals(AbstractStringBuilder)`
> bytecode, which reads the builder's `getCoder()`/`getValue()` fields directly.
> CratonVM backs `StringBuilder` with a **synthetic layout** (`slot 0 = char[]
> buffer`, `slot 1 = int count`) that does **not** match the JDK
> `AbstractStringBuilder` layout (`byte[] value`, `byte coder`, `int count`). So
> `getCoder()` reads the synthetic `int count` (e.g. 21) as the coder byte → it is
> `!= LATIN1`, forcing the `StringUTF16.contentEquals(byte[] v1, byte[] v2, int len)`
> branch; there `v2` is the char[] buffer mis-typed as a UTF16 `byte[]` whose
> reported length is half what `len` requires, so `checkBoundsOffCount(0, len, v2)`
> throws `StringIndexOutOfBoundsException`.

**Severity:** Medium-High (any real-JDK code path that calls
`String.contentEquals(StringBuilder/AbstractStringBuilder)` — or otherwise reads
a `StringBuilder`'s `getValue()`/`getCoder()`/`value`/`coder` via real bytecode —
crashes or misbehaves; CORS same-origin checks are one user, but the layout
mismatch is general).
**Status on CratonVM:** ✅ FIXED (dev `c8273be8`, 2026-06-23). Added
`AbstractStringBuilder.getValue()[B` / `getCoder()B` natives in
`native-builtins/src/lang_string.rs` (`native_sb_get_value` / `native_sb_get_coder`,
registered via `register_string_builder_natives` on StringBuilder/StringBuffer/
AbstractStringBuilder). `getCoder` derives the coder exactly as
`vm_object::create_java_string` (LATIN1 iff all units ≤ 0xFF, else UTF16);
`getValue` packs the synthetic `char[]` into a compact little-endian `byte[]`
matching CratonVM's String layout, so the real `nonSyncContentEquals` path
computes correctly on every coder combination. `TestCorsFilter` 5 FAIL → 83/83
PASS (jit + nojit); `contentEquals` repro battery byte-identical to HotSpot;
+6 unit tests, 2695 native-builtins tests green.
**Was:** FAIL (5 of 83 tests throw / assert-fail). **HotSpot:** PASS (83/83).
**Run date:** 2026-06-22
**Binary:** dev `df11ac00` (worktree `C:\craton\CratonVM-tctest`, branch `chore/tomcat-fullsuite-20260622`).

## Affected classes / tests

`org.apache.catalina.filters.TestCorsFilter` — 5 failures:

- `testDoFilterSameHostWithOrigin01` (SIOOBE)
- `testDoFilterSameHostWithOrigin03` (SIOOBE)
- `testDoFilterSameHostWithOrigin05` (SIOOBE)
- `testDoFilterSameHostWithOrigin06` (downstream `AssertionError: expected:<NOT_CORS> but was:<SIMPLE>` — the contentEquals returned the wrong answer / the host comparison silently mis-evaluated)
- `testDoFilterSameHostWithOrigin07` (SIOOBE)

Underlying VM defect is in the **`StringBuilder` object model**, not in Tomcat.

## Symptom

`CorsFilter.checkRequestType` (line 555) calls
`RequestUtil.isSameOrigin(request, origin)`, whose last line (RequestUtil.java:176)
is `return origin.contentEquals(target);` where `target` is a `StringBuilder`
built by appending `scheme://host[:port]`. On CratonVM this throws:

```
java.lang.StringIndexOutOfBoundsException
    at java.lang.StringUTF16.checkBoundsOffCount(StringUTF16.java:1662)
    at java.lang.StringUTF16.contentEquals(StringUTF16.java:1512)
    at org.apache.tomcat.util.http.RequestUtil.isSameOrigin(RequestUtil.java:176)
    at org.apache.catalina.filters.CorsFilter.checkRequestType(CorsFilter.java:555)
    at org.apache.catalina.filters.TestCorsFilter.doTestDoFilterSameHostWithOrigin(TestCorsFilter.java:470)
    at org.apache.catalina.filters.TestCorsFilter.testDoFilterSameHostWithOrigin03(TestCorsFilter.java:432)
    ...
```

(`testDoFilterSameHostWithOrigin01` shows the same stack with `isSameOrigin`
inlined / a slightly different leaf; all five share the `StringUTF16.contentEquals`
→ `checkBoundsOffCount` frame.) The origins are pure ASCII
(`"http://localhost:8080"`, `"https://localhost:8443"`, `"http://localhost"`,
etc.), so on HotSpot both operands are LATIN1 and the comparison never even
reaches `StringUTF16.contentEquals`.

## Root cause analysis

### The real-JDK dispatch (JDK 25, verified from `lib/src.zip`)

`String.contentEquals(CharSequence cs)` → for an `AbstractStringBuilder`
(StringBuilder/StringBuffer) → `nonSyncContentEquals(AbstractStringBuilder sb)`:

```java
// java.lang.String  (String.java:1951)
private boolean nonSyncContentEquals(AbstractStringBuilder sb) {
    int len = length();
    if (len != sb.length()) return false;
    byte[] v1 = value;
    byte[] v2 = sb.getValue();          // <-- reads sb.value field
    byte coder = coder();
    if (coder == sb.getCoder()) {       // <-- reads sb.coder field
        return v1.length <= v2.length && ArraysSupport.mismatch(v1, v2, v1.length) < 0;
    } else {
        if (coder != LATIN1) return false;
        return StringUTF16.contentEquals(v1, v2, len);   // String.java:1965
    }
}
```

```java
// java.lang.StringUTF16  (StringUTF16.java:1511)
public static boolean contentEquals(byte[] v1, byte[] v2, int len) {
    checkBoundsOffCount(0, len, v2);    // line 1512  <-- THROWS
    ...
}
// StringUTF16.java:1661
public static void checkBoundsOffCount(int offset, int count, byte[] val) {
    String.checkBoundsOffCount(offset, count, length(val));   // length(val) == val.length >> 1
}
```

On HotSpot, for these ASCII origins, `origin.coder == LATIN1` **and**
`sb.getCoder() == LATIN1`, so execution takes the `coder == sb.getCoder()`
fast path (line 1960, `ArraysSupport.mismatch`) and `StringUTF16.contentEquals`
is never called. `TestCorsFilter` passes.

### Why CratonVM diverges

CratonVM does **not** model `StringBuilder` with the real
`AbstractStringBuilder` field layout. Its synthetic layout is, per
`native-builtins/src/lang_string.rs::sb_state` (and confirmed across every SB
native and the `String(StringBuilder)` ctor in `deprecated_util.rs`):

```
slot 0 : char[]  buffer       (NOT a compact-string byte[])
slot 1 : int     count
```

whereas the JDK `AbstractStringBuilder` is:

```
byte[] value;     // compact-string bytes (LATIN1 1B/char or UTF16 2B/char)
byte   coder;     // 0 = LATIN1, 1 = UTF16
int    count;
```

`getValue()` and `getCoder()` are **final accessors on `AbstractStringBuilder`
with no CratonVM native override** (grep confirms: no native registered for
`getValue`/`getCoder`/`coder`), so `nonSyncContentEquals` runs them as real
bytecode against the synthetic object:

1. **`sb.getCoder()` reads the wrong field.** It returns whatever sits where the
   JDK expects `coder`. In the synthetic layout that is the `int count` field
   (slot 1), e.g. `21` for `"http://localhost:8080"`. Read as the `coder` byte
   this is `!= LATIN1(0)`, so `coder == sb.getCoder()` is **false** → the code
   falls into the `else` branch and, because `origin.coder == LATIN1`, calls
   `StringUTF16.contentEquals(v1, v2, len)`.

2. **`sb.getValue()` returns the synthetic `char[]` mis-typed as `byte[]`.** In
   `StringUTF16.contentEquals`, `checkBoundsOffCount(0, len, v2)` computes
   `length(v2) = v2.length >> 1`. The synthetic buffer's element count is the
   char capacity (≈ `len`), not `2*len` bytes, so `length(v2) ≈ len/2 < len` →
   `offset(0) + count(len) > length(v2)` → **`StringIndexOutOfBoundsException`**.
   (`StringUTF16.getChar`, which reads pairs of bytes, would also read garbage
   if the bounds check did not fire first.)

This is the **same synthetic-`StringBuilder`-vs-real-`AbstractStringBuilder`-layout
family** already documented for `BUG-M` (`StringBuilder.lastIndexOf` ran real
`AbstractStringBuilder.lastIndexOf` bytecode against the synthetic buffer and
returned -1) and called out in `BUG-DF05`. Here the victim is
`String.nonSyncContentEquals` reading `getValue()`/`getCoder()`. The
`Origin06` `AssertionError` (no exception, wrong CORS classification) is the same
root cause producing a *silently wrong* answer instead of an SIOOBE, because the
length/coder mismatch made the comparison return the wrong boolean for that input.

### Where the fix must go

There is no `StringUTF16.contentEquals` native to "fix" — the JDK method is
correct; it is being fed a synthetic object that violates the
`AbstractStringBuilder` contract. The fix belongs in the **StringBuilder object
model / native layer** in `native-builtins/src/lang_string.rs`, one of:

- **Preferred (mirrors BUG-M / the existing `indexOf`/`lastIndexOf` natives):**
  register natives for `java/lang/AbstractStringBuilder.getValue()[B` and
  `getCoder()B` (and ideally `String.contentEquals(Ljava/lang/CharSequence;)Z` /
  `nonSyncContentEquals`) that read the synthetic `char[]` buffer + `count` and
  produce a *correct* compact-string `byte[]` view + coder, so the real
  `nonSyncContentEquals` path computes correctly. A `contentEquals(CharSequence)`
  native that compares CratonVM's synthetic chars against the receiver String
  directly is the most robust (it bypasses the whole `getValue`/`getCoder`
  expectation), matching how `indexOf`/`lastIndexOf`/`append(...)` are already
  intercepted.
- **Structural (larger):** back `StringBuilder` with a real compact-string
  `byte[] value` + `byte coder` so all unregistered real-JDK
  `AbstractStringBuilder` bytecode (getValue/getCoder/charAt/codePointAt/…) works
  without per-method natives. This would also retire the per-method native
  patchwork (BUG-M, `String(StringBuilder)` ctor, etc.) but is a much bigger change.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
$cp  = (Get-Content .tooling\cp.txt -Raw).Trim()
$exe = "C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe"
$env:CRATONVM_REAL_NET_SOCKETS = "1"
$env:CRATONVM_REAL_AQS = "1"
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG = "1"
& $exe -Xmx2g -cp $cp org.junit.runner.JUnitCore org.apache.catalina.filters.TestCorsFilter
# Expect: Tests run: 83, Failures: 5 (5x StringIndexOutOfBoundsException / NOT_CORS-vs-SIMPLE)
```

Minimal standalone repro (mirrors `isSameOrigin`: a `String` compared against a
`StringBuilder` `CharSequence`):

```java
public class CeRepro {
    public static void main(String[] a) {
        String origin = "http://localhost:8080";          // LATIN1 String
        StringBuilder target = new StringBuilder();
        target.append("http").append("://").append("localhost");
        target.append(':').append(8080);                  // -> "http://localhost:8080"
        // HotSpot: true. CratonVM: StringIndexOutOfBoundsException from
        // StringUTF16.contentEquals -> checkBoundsOffCount.
        System.out.println(origin.contentEquals(target));

        // Also exercise the inequality / wrong-answer side (Origin06-style):
        System.out.println("https://localhost".contentEquals(
            new StringBuilder("https://localhost")));      // HotSpot: true
    }
}
```

Run the repro with the same binary:
`& $exe -cp . CeRepro` — HotSpot prints `true`/`true`; CratonVM throws SIOOBE on
the first line.

## Duplicate / relationship check

- **Not a duplicate.** No existing `BUG-*.md` covers `String.contentEquals` /
  `StringUTF16.contentEquals` / `checkBoundsOffCount`.
- **Same root-cause family as `BUG-M`** (`StringBuilder.lastIndexOf` — synthetic
  SB layout read by real `AbstractStringBuilder` bytecode) and noted in
  `BUG-DF05` (char[]-backed StringBuilder vs byte[]-assuming
  `String(AbstractStringBuilder, Void)` ctor). Both were fixed with targeted
  natives in `lang_string.rs`; this is the next method (`getValue`/`getCoder`
  via `nonSyncContentEquals`) in that same gap.
- **Not DF02** (stale/zeroed-OOP JIT-root GC bug): the failure is
  deterministic on every run, every affected test, and is a clean
  `StringIndexOutOfBoundsException` from real JDK bytecode with arithmetic that
  follows directly from the layout mismatch — no GC-guard fingerprints, no
  ~1/N timing dependence.

## Recommendation

**FIX (bounded VM fix).** This is a self-contained object-model gap, not a
GC/JIT/threading defect. Add `AbstractStringBuilder.getValue()[B` /
`getCoder()B` natives (or a `String.contentEquals(CharSequence)` native) in
`native-builtins/src/lang_string.rs`, exactly mirroring the existing
`indexOf`/`lastIndexOf` synthetic-buffer natives (BUG-M precedent). Low risk,
high value: it closes a general correctness hole — any real-JDK code reading a
`StringBuilder`'s value/coder (very common) currently crashes or returns wrong
results — and turns `TestCorsFilter` 5 FAIL → PASS. The structural fix (real
`byte[] value`/`byte coder` StringBuilder backing) is the longer-term
alternative that would retire this whole class of per-method natives, but is out
of scope for this defect.
