# keycloak #15 — `java.nio.file.Path` Windows drive/UNC root parsing (root/name ✅ FIXED; UNC-join residual)

**Status:** **`getRoot`/`getNameCount`/`getName`/`subpath` ✅ FIXED** (2026-06-18, branch
`fix/keycloak-15-path-root`) — resolves **testFiles** (the `getRoot()==null` NPE). One residual
(**testResource**) remains: `Path.of("/", x)` does not produce the JDK UNC `\\x\` form — that needs
faithful `WindowsPathParser` join semantics in the (pervasive, higher-risk) `Paths.get` join, deferred.

**Fix (verified):** `getRoot`/`getNameCount`/`getName`/`subpath` now parse the Windows drive/UNC/verbatim
prefix explicitly (helper `p57_parse_win_root` in `native-builtins/src/phases_late.rs`, and the mirror
`parse_windows_path_root` in `native-io/src/lib.rs`) instead of relying on `std::path::Component`, which in
this build leaves `C:` as a `Normal` component. Verified == HotSpot for drive-abs/drive-fwd/drive-rel/UNC/
root-rel/relative/dot-abs (`repros/keycloak-15-path-root/PathRoot.java`, plus a `subpath`/`resolve`/
`normalize`/`startsWith`/`Files.exists` regression check — all match HotSpot). **Root cause of the
non-dispatch trap:** the live handlers are the p57 closures over a `/`-canonical string (the old getRoot
only checked for a `\` at index 2, so it never matched the drive).

**Affected (CV-only, HotSpot passes):**
- `org.keycloak.theme.ResourceLoaderTest` — was 2/2 fail; **`testFiles` (getRoot NPE) now fixed**;
  `testResource` (UNC-join content) still open (residual above).

## Symptom
```
java.lang.NullPointerException: Cannot invoke equals on null
  at org.keycloak.theme.ResourceLoaderTest.testFiles(ResourceLoaderTest.java:55)
```
`testFiles` does `Paths.get(".").toAbsolutePath().getRoot().equals(...)` — `getRoot()`
returns **null** on CV. `testResource` resolves the wrong resource name because
`Path.of("/", root).normalize().toAbsolutePath()` then `substring(2)` operates on a
single-backslash string instead of the JDK's UNC `\\…` form.

## Root cause (confirmed, re-verified on the dev-merged binary)
CV's `java.nio.file.Path` natives do **not** parse the Windows drive/UNC prefix.
Standalone diff (`scratch/path/P.java`):

| call | HotSpot | CratonVM |
|------|---------|----------|
| `Paths.get(".").toAbsolutePath().getRoot()` | `C:\` | **`null`** |
| `Paths.get("C:\\x\\y").getRoot()` | `C:\` | **`null`** |
| `Paths.get("C:\\x\\y").getNameCount()` | `2` | **`3`** (counts `C:` as a name) |
| `Path.of("/", "dummy-resources/parent")` | `\\dummy-resources\parent\` | `\dummy-resources\parent` |

The natives in `native-io/src/lib.rs` rely on `std::path::Path::components()` to
classify the prefix:
- `native_path_get_root` (~line 7945) — matches `Component::RootDir | Prefix(_)`;
  returns null when neither is produced.
- `native_path_get_name_count` (~8155) / `native_path_get_name` (~8169) — count
  `Component::Normal` only.
- `native_paths_get` (~7869) — naive join with `MAIN_SEPARATOR`, no Windows
  drive/UNC root semantics (the `Path.of("/", x)` divergence).

Empirically, `Component::Prefix` is **not** produced for `C:\…` strings here
(name count is 3, i.e. `C:` is treated as a `Normal` component while `\` is still a
separator) even though `std::fs::canonicalize` elsewhere yields proper `C:\…`
paths — so the prefix classification these natives depend on is unreliable in this
build. Not a JIT bug (`--nojit` identical).

## Fix direction
Implement explicit Windows path-root parsing in the `nio.file.Path` natives instead
of delegating prefix classification to `std::path::Component`:
- detect a `<letter>:` drive prefix and `\\server\share` UNC root so `getRoot`
  returns `C:\`, and `getNameCount`/`getName` exclude the drive/root component;
- reproduce `sun.nio.fs.WindowsPath` join semantics in `native_paths_get`
  (notably `Path.of("/", x)` → UNC `\\x`).
No synthetic stub — these natives must match WindowsPath parsing. A regression home
already exists at `vm/tests/kc26_path_conformance.rs`.

**Caution:** `java.nio.file.Path` is pervasive; verify broadly (existing path/io
suites + bt-style runs) before landing, and prefer additive prefix handling over
rewriting the component model.

## Repro
`scratch/path/P.java`:
```
& "C:/Program Files/Java/jdk-25/bin/java.exe" -cp <dir> P            # HotSpot: root=C:\ nameCount=2
& cratonvm-kcfull.exe --java-home <jdk> -cp <dir> P                  # CV: root=null nameCount=3
```
Full test: `powershell -File apps/keycloak/repro-kc.ps1 org.keycloak.theme.ResourceLoaderTest`
