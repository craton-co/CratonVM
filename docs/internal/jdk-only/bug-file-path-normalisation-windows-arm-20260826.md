# `java.io.File`'s Windows normalizer collapsed no separators and stripped no trailing one — so `new File("a/")` did not equal `new File("a")`

**Status: FIXED 2026-08-26** for the normalization, `isAbsolute`, and the two
parent-resolution rules. **14 differences remain**, in three narrower families
listed in §5. Present in **both** modes.

## 1. How it was found

`java/io/File` is 54 of the 2,057 `bridge`-kind rows in the `--jdk-only`
retirement surface. Before proposing any retirement, the rule from
`the-retirement-surface-is-2446-…` says probe the family against the oracle.
`probes/FilePathSweep.java` sweeps the PATH-STRING surface — `getName`,
`getParent`, `getPath`, `isAbsolute`, `toString`, `hashCode`, `toURI`,
`compareTo`, `equals`, both constructors — over 39 path shapes. No filesystem
access, so it is a pure two-VM stdout diff.

```text
HotSpot 25.0.3+9   666 lines
CratonVM           62 differing lines, IDENTICAL in both modes
```

Mode-identical means this was never a `--jdk-only` defect. It is a plain
conformance defect that the jdk-only survey happened to walk into.

## 2. One root cause, wide fallout

`file_normalise_path_units`' `#[cfg(windows)]` arm did two of
`WinNTFileSystem.normalize`'s jobs and neither of the other two:

* it **never collapsed runs of separators** — `a//b` → `a\\b`;
* its trailing-separator loop was guarded by `while normalized.len() > 3`, which
  protects the drive root `C:\` and **also every short relative path** — so
  `a/` → `a\`.

That one surviving character is why `getName()` answered `""`, `getParent()`
answered `"a"`, and — the sharp one —

```java
new File("a/").equals(new File("a"))   //  HotSpot: true    CratonVM: false
```

**A `File` that is not equal to itself-without-a-trailing-slash breaks any code
using it as a map key or de-duplicating paths**, and `hashCode`, `compareTo`,
`toString` and `toURI` all inherited the same wrong string.

**The `#[cfg(not(windows))]` sibling had always been correct** — it collapses
runs and strips the trailing separator, under a comment naming the Spring
failure that forced it (`PathMatchingResourcePatternResolver` building
`.../scanned//*.txt`, which matches nothing). The two arms of one function
simply disagreed and the Windows one was the wrong half. That is the fourth
instance of this shape this week, after `getPeakThreadCount`, `setDoOutput`, and
`huc_get_request_property`.

## 3. Three more rules were missing

Found by re-running the probe after each fix rather than by reading:

* **`File.isAbsolute` delegated to Rust's `Path::is_absolute`**, which is not
  the same predicate. `WinNTFileSystem.isAbsolute` accepts a UNC path
  (prefix `\\`) and rejects drive-relative `C:foo`; Rust wants a prefix AND a
  root, so `new File("//").isAbsolute()` answered `false` against HotSpot's
  `true`. Now implements the JDK's rule directly, deliberately NOT shared with
  `p57_win_is_absolute` — `java.nio.file.Path` genuinely disagrees here.
* **An empty parent is not "no parent".** The JDK resolves
  `File(String parent, String child)` as
  `fs.resolve(fs.getDefaultParent(), fs.normalize(child))`, and
  `getDefaultParent()` is `\` on Windows, `/` on Unix — so `new File("", "kid")`
  is `\kid`, ROOTED. It was returning the relative `kid`.
* **A ROOT parent must keep its separator.** Stripping the parent's trailing
  separators unconditionally destroyed the UNC prefix: `new File("//", "kid")`
  came out `\kid`, a network path demoted to a driveless-rooted one. The fix
  normalises the parent first, then appends a separator only when the parent
  does not already end in one — which is exactly the root test, because
  normalization has already removed every non-root trailing separator.

23 rows of regression test cover all four, `#[cfg(windows)]`, beside the
existing Unix ones.

## 4. Sixteen of the "differences" were the PROBE

At 30 remaining the diff still showed the CJK path row. It was not a defect:

```text
HotSpot   [unicode/???] getName |???|          stdout.encoding=Cp1251
CratonVM  [unicode/<utf-8 bytes>] getName …    stdout.encoding=UTF-8
```

Identical values, different console encoding — **the diff was a function of each
VM's stdout, not of its behaviour.** The probe now escapes every non-ASCII
character as a hex escape before printing, which is the general fix: a
cross-VM stdout diff must not carry an encoding-dependent byte.

That correction moved the measured residual from 30 to **14**, so the honest
arc is **46 true differences → 14**, not 62 → 30.

## 5. What remains — 14 differences, three families

* **`toURI` (8).** `file:/C://` where HotSpot gives `file:////`;
  `file://server/share` where HotSpot gives `file:////server/share`; and it
  percent-encodes non-ASCII where HotSpot does not. A separate defect in the URI
  construction, not in normalization.
* **`getParentFile` (4).** `a/./b` → `a` where HotSpot keeps `a\.`; `null` for a
  UNC parent where HotSpot gives `\\server`; `null` for `/.` where HotSpot gives
  `\`.
* **Bare drive as parent (2).** `new File("C:", "kid")` → `C:\kid` where HotSpot
  gives the drive-RELATIVE `C:kid`. `C:` is a prefix but not a root, so the
  "ends in a separator" rule of §3 does not reach it.

None is fixed here, and none is a regression from this change — all fourteen
were in the original 62.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp probes/out FilePathSweep
```
