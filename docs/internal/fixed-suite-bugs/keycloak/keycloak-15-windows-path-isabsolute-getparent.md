# keycloak-15 (residuals) — Windows `Path` isAbsolute / getParent / normalize / relativize ✅ FIXED

| | |
|---|---|
| **Status** | ✅ **FIXED** 2026-06-21 (dev). The original `getRoot()=null` half was fixed earlier; this closes the residuals surfaced by `PathProbe` + `PathDeep`. |
| **Kind** | VM correctness — synthetic `java.nio.file.Path` (`sun.nio.fs.WindowsPath`) semantics |
| **Surfaced by** | `docs/known-issues/repros/keycloak-15-path-root/{PathProbe,PathDeep}.java` (Quarkus/Keycloak path handling) |
| **CratonVM (before)** | drive-relative / driveless-rooted paths reported absolute; `getParent()` over-trimmed a trailing `.`; `normalize()` dropped the root / leading `..`; `relativize()` couldn't backtrack with `..` |
| **HotSpot** | the spec we matched (JDK 25) |
| **Remaining** | `equals()`/`hashCode()` are case-sensitive; HotSpot WindowsPath is **case-insensitive** (`C:\A`.equals(`c:\a`)==true). Deferred — needs equals+hashCode+compareTo+startsWith/endsWith aligned. |

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

In `../../../../native-builtins/src/phases_late.rs`, the `p57` synthetic Path natives (which win over
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

## Second wave — `normalize()` / `relativize()` (surfaced by `PathDeep`)

A deeper sweep (`PathDeep.java`) found two more deterministic divergences:

| Case | HotSpot | CratonVM (before) |
|---|---|---|
| `C:\a\..\..\b`.normalize() | `C:\b` | `b` (dropped the drive root) |
| `C:\..`.normalize() | `C:\` | `` (empty) |
| `..\..\a`.normalize() | `..\..\a` | `a` (dropped leading `..`) |
| `C:\a\b`.relativize(`C:\a\x`) | `..\x` | `C:\a\x` (no `..` backtrack) |
| `a\b\c`.relativize(`a\b`) | `..` | `a\b` |

Root cause: `p57_normalize_path` split on `/` and popped `..` unconditionally — so a `..`
popped the drive root, and a leading `..` on a relative path was dropped instead of kept.
`relativize` used `strip_prefix`, which only handles the case where `target` is *under*
`base` and otherwise returned `target` unchanged.

Fix: make both root-aware off `p57_parse_win_root`.
* `p57_normalize_path` — keep the parsed root, and when a `..` has nothing to cancel,
  **discard** it under a root (can't go above it) but **keep** it on a relative path.
* `p57_relativize` — emit `..` × (base-tail length) + target-tail off the shared root;
  returns `None` (→ caller falls back to `target`) when roots differ / absoluteness mismatches.
Routed both `normalize` registrations (the winning inline one too) and the non-jar
`relativize` through them; the jar-FS `relativize` keeps its `strip_prefix` path.

## Verification

* `PVerify.java` (new repro): 14/14 cases byte-identical to HotSpot (JDK 25).
* `PathProbe.java`: byte-identical to HotSpot **except** the deliberately-scoped-out
  `of-slash-x` UNC-construction line.
* `RegrPath.java`: common ops (`resolve`/`normalize`/`getFileName`/`resolveSibling`/
  `startsWith`/`getParent` of normal paths) unchanged, == HotSpot.
* `PathDeep.java` (new repro): byte-identical to HotSpot **except** the `equals`
  case-insensitivity line (deferred — see *Remaining* above).
* Pure-function unit tests: `cargo test -p cratonvm-native-builtins p57_win_path_tests
  p57_normalize_relativize_tests` (`is_absolute_matches_hotspot`,
  `parent_keeps_curdir_and_root_boundary`, `normalize_preserves_root_and_leading_dotdot`,
  `relativize_backtracks_with_dotdot`).
* Path conformance KC26.1–7 green; native-builtins path/p57 tests green.

## Repro

```
$JDK=…/jdk-25 ; CV=target/release/cratonvm.exe
javac -d out docs/known-issues/repros/keycloak-15-path-root/PVerify.java
"$JDK/bin/java"  -cp out PVerify          # HotSpot baseline → RESULT=OK
"$CV" --java-home "$JDK" -cp out PVerify  # CratonVM         → RESULT=OK (was FAIL)
```
