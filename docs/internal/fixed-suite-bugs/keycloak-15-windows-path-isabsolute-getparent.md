# keycloak-15 (residuals) — Windows `Path.isAbsolute()` / `getParent()` ✅ FIXED

| | |
|---|---|
| **Status** | ✅ **FIXED** 2026-06-21 (dev). The original `getRoot()=null` half was fixed earlier; this closes the residuals surfaced by `PathProbe`. |
| **Kind** | VM correctness — synthetic `java.nio.file.Path` (`sun.nio.fs.WindowsPath`) semantics |
| **Surfaced by** | `docs/known-issues/repros/keycloak-15-path-root/PathProbe.java` (Quarkus/Keycloak path handling) |
| **CratonVM (before)** | drive-relative / driveless-rooted paths reported absolute; `getParent()` over-trimmed a trailing `.` |
| **HotSpot** | the spec we matched (JDK 25) |

## Symptom

After the keycloak-15 `getRoot()`/`getNameCount()` drive-parsing fix landed, the bare
`PathRoot` repro passed but the thorough `PathProbe` still diverged from HotSpot on three
deterministic, pure-logic cases (Windows):

| Case | HotSpot | CratonVM (before) |
|---|---|---|
| `C:foo` (drive-relative) `isAbsolute` | `false` | `true` ❌ |
| `\foo\bar` (driveless-rooted) `isAbsolute` | `false` | `true` ❌ |
| `C:\a\b\.` (trailing `.`) `getParent` | `C:\a\b` | `C:\a` ❌ |

## Root cause

In `native-builtins/src/phases_late.rs`, the `p57` synthetic Path natives (which win over
the `native-io` ones — last registration wins) used two flawed shortcuts:

* **`isAbsolute`** — `p.starts_with('/') || p[1]==':'`. That treats *any* rooted or
  drive-prefixed path as absolute. But on Windows a **drive-relative** path (`C:foo`,
  relative to the current dir on drive C) and a **driveless-rooted** path (`\foo`, relative
  to the current drive) both have a root component yet are **not** absolute. A WindowsPath is
  absolute only when it has both a drive *and* a root (`C:\…`) or is a UNC path
  (`\\server\share\…`).
* **`getParent`** — delegated to Rust `std::path::Path::parent()`, which **normalizes a
  trailing `.` away** before splitting, so it over-trims: parent of `C:\a\b\.` became `C:\a`
  instead of `C:\a\b`. HotSpot's `WindowsPath.getParent()` is a pure last-separator split
  that keeps `.`/`..` name elements verbatim.

The internal stored string is `/`-canonical (`p57_alloc_path` folds `\`→`/` on Windows), so
`\foo\bar` is stored as `/foo/bar` — which is why the `starts_with('/')` arm also fired for
driveless-rooted paths.

## Fix

Two pure helpers, classifying off the already-correct `p57_parse_win_root` (root + names),
keep `isAbsolute`/`getParent` consistent with `getRoot`/`getNameCount`/`getName`:

* `p57_win_is_absolute` — absolute iff the parsed root is `C:\` (drive + separator) or UNC.
* `p57_win_parent_of` — rebuilds the parent from `(root, names[..n-1])`, keeping `.`/`..`;
  a drive-relative root (`C:`) attaches the first name with no separator
  (`C:foo\bar` → `C:foo`). Returns "" (→ `null`) for a root-only/single-element/relative path.

Both `isAbsolute` registrations, `p57_parent_of` (also feeds `resolveSibling`), and the
winning (last-registered) `getParent` route through them. Unix keeps the POSIX leading-`/`
rule. The **`Path.of("/", x)` UNC-construction quirk** (`of-slash-x` in `PathProbe`) is left
as-is: faithfully replicating it needs a change to the core `Paths.get(first, more…)`
separator-join used everywhere, for one obscure case — not worth the regression surface.

## Verification

* `PVerify.java` (new repro): 14/14 cases byte-identical to HotSpot (JDK 25).
* `PathProbe.java`: byte-identical to HotSpot **except** the deliberately-scoped-out
  `of-slash-x` UNC-construction line.
* `RegrPath.java`: common ops (`resolve`/`normalize`/`getFileName`/`resolveSibling`/
  `startsWith`/`getParent` of normal paths) unchanged, == HotSpot.
* Pure-function unit tests: `cargo test -p cratonvm-native-builtins p57_win_path_tests`
  (`is_absolute_matches_hotspot`, `parent_keeps_curdir_and_root_boundary`).
* Path conformance KC26.1–7 green; 21 native-builtins path/p57 tests green.

## Repro

```
$JDK=…/jdk-25 ; CV=target/release/cratonvm.exe
javac -d out docs/known-issues/repros/keycloak-15-path-root/PVerify.java
"$JDK/bin/java"  -cp out PVerify          # HotSpot baseline → RESULT=OK
"$CV" --java-home "$JDK" -cp out PVerify  # CratonVM         → RESULT=OK (was FAIL)
```
