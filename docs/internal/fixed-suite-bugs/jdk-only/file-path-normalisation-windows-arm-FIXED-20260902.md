# The Windows `java.io.File` path surface — 0 differing lines, both modes

Retires `internal/jdk-only/bug-file-path-normalisation-windows-arm-20260826.md`
(which `03d2bd990` had bulk-archived from `known-issues/` unchanged).

## Status

**FIXED 2026-09-02.** `probes/FilePathSweep.java`, 666 lines, against HotSpot
25.0.3+9 on Windows 11:

| | differing lines |
|---|---|
| the page's residual (2026-08-26) | 14 |
| current dev, measured before touching anything | **9** |
| after this change, `--jdk-only` | **0** |
| after this change, compatible | **0** |

The page's honest arc was `46 true differences -> 14`. It is now
**46 -> 0**.

## 0. Five of the fourteen were already gone

The page's §5 listed three families. Measured on dev `eb79f5904` before writing
any code:

* **`getParentFile` (4 rows) — already fixed.** `08bbcc0bd` routed both
  `getParent` and `getParentFile` through `file_parent_units`, two days AFTER
  this page was written. Its commit comment names the same two shapes the page
  does (`a/./b`, `/.`).
* **`toURI` percent-encoding non-ASCII (1 row) — already fixed.**
  `encode_file_uri_path` now carries an explicit "NON-ASCII IS LEFT LITERAL"
  rule, with the same `unicode/é中文` measurement in its own comment.

Reading a residual list is not the same as measuring it. Both of those would
have been "fixed" a second time.

## 1. What was actually left, and why it was one cause and not two

Nine rows, and eight of them come from a single wrong predicate:

```text
[//] toURI                HotSpot file:////              CratonVM file:/C://
[///] toURI               HotSpot file:////              CratonVM file:/C://
[\\] toURI                HotSpot file:////              CratonVM file:/C://
[C:] toURI                HotSpot file:/C:/craton/CratonVM/   CratonVM file:/C:/
[\\server\share] toURI    HotSpot file:////server/share  CratonVM file://server/share
[\\server\share\f] toURI  HotSpot file:////server/share/f CratonVM file://server/share/f
[//server/share] toURI    HotSpot file:////server/share  CratonVM file://server/share
[C:] child                HotSpot C:kid                  CratonVM C:\kid
[C:] twoArg               HotSpot C:kid                  CratonVM C:\kid
```

### 1.1 `Path::is_absolute` is not `File.isAbsolute`

`toURI` absolutized with `std::path::Path::is_absolute`, which wants a prefix
**and** a root. A UNC path has a root and no prefix, so Rust calls it relative
and `Path::join` resolves it against the working directory — keeping the CWD's
drive and discarding the server. `\\` became `C:\\`, which slashifies to
`C://`, which is the `file:/C://` in rows 1-3.

The tree already had the right predicate: `file_is_absolute`, written for
`File.isAbsolute` by this page's own §3. The two questions are the same
question, so they are now the same answer.

### 1.2 Windows keeps a working directory per drive

`C:` is a prefix without a root — drive-RELATIVE. HotSpot resolves it against
the process's directory **on drive C**, which is why `new File("C:").toURI()`
is `file:/C:/craton/CratonVM/` and not `file:/C:/`. Neither `Path::join` nor
`current_dir()` can express that; `GetFullPathNameW` on the bare `X:` is the
platform's own answer and is what the JDK reaches for.

It is deliberately scoped to that shape alone. `GetFullPathNameW` also
collapses `.` and `..`, and `getAbsolutePath` is **not** `getCanonicalPath` —
HotSpot keeps `a\..\b`, and the probe has a row (`[a/../b]`) that was already
green and had to stay green.

### 1.3 `File.toURI()` has a line CratonVM did not

```java
String sp = slashify(f.getPath(), f.isDirectory());
if (sp.startsWith("//")) sp = "//" + sp;      // <- this one
return new URI("file", null, sp, null);
```

A URI whose scheme-specific part opens with `//` parses the next segment as an
**authority**. Without the doubling, `//server/share` gives host `server` and
path `/share`; with it the authority is empty and the whole thing stays in the
path. They do not merely render differently — `getHost()` answers `server` on
one and `null` on the other, so a round trip through `new File(uri)` reaches a
different place.

### 1.4 A bare drive takes no separator

`WinNTFileSystem.resolve` names this case itself:

```java
boolean isDirectoryRelative =
    pn == 2 && isLetter(parent.charAt(0)) && parent.charAt(1) == ':';
```

and when it holds it copies the child straight after the parent. `C:kid` is in
drive C's working directory; `C:\kid` is in its root, and those coincide only
when the process's directory on C: happens to be the root. Windows-only: on
Unix `C:` is an ordinary relative name and HotSpot answers `C:/kid`, which is
what this VM already did there.

## 2. The fix is one shared rule, not three patches

`toURI`, `getAbsolutePath` and `getAbsoluteFile` each carried their own copy of
`if p.is_absolute() { .. } else { cwd.join(..) }`. Fixing only `toURI` would
have left the copies disagreeing — and **`File.toURI()` is defined in terms of
`getAbsoluteFile()`**, so a UNC or drive-relative path that absolutizes
differently in the two places is a `File` whose own URI names a different file.

That is this page's §2 shape exactly ("the two arms of one function simply
disagreed and the Windows one was the wrong half"), which is why the new
`file_absolutize` is a single function all three call, rather than three edits.

The other two rows are `file_join_parent_child_units`, one condition.

| change | rows |
|---|---|
| `file_absolutize` — `File`'s absolutization rule, shared by `toURI` / `getAbsolutePath` / `getAbsoluteFile` | 4 |
| the JDK's `//` doubling in `toURI` | 3 |
| `isDirectoryRelative` in `file_join_parent_child_units` | 2 |

## 3. Regression cover

Four rows added to the existing `#[cfg(windows)]`
`windows_default_parent_and_unc_absolute`, beside the ones this page's §3 left:

```rust
assert_eq!(j("C:",  "kid"), "C:kid");    // the rule
assert_eq!(j("C:/", "kid"), "C:\\kid");  // a drive WITH a root is a root
assert_eq!(j("ab",  "kid"), "ab\\kid");  // two chars, not a drive
assert_eq!(j("1:",  "kid"), "1:\\kid");  // two chars, not a LETTER
```

The last three are the boundaries: they fail if the rule is widened past
exactly `<letter>:`.

## 4. What this does not claim

* The sweep is the PATH-STRING surface only. It deliberately excludes
  `exists()` / `length()` / `lastModified()`, so nothing here says anything
  about filesystem access.
* `[C:] toURI` is CWD-dependent by construction — HotSpot's answer names the
  directory the probe was run from. Both VMs were run from the same working
  directory; a run from elsewhere changes the expected line, not the verdict.
* The two modes were measured separately and both are 0. Mode-identical is what
  this page said from the start: it was never a `--jdk-only` defect.

## Reproduce

```bash
javac -d probes/out probes/FilePathSweep.java
java -cp probes/out FilePathSweep > hotspot.txt
cratonvm --java-home "$JDK" --jdk-only -cp probes/out FilePathSweep > cvm.txt
diff hotspot.txt cvm.txt
```
