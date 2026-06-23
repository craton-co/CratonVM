# Bug X — `JarFile/ZipFile.size()` returns 0 and `.entries()` returns null while the archive is open (annotation-scan "zip file is empty")

**Severity:** Low–Medium (mostly a non-fatal WARN). Affects **63** suite classes
(0 on HotSpot): `StandardJarScanner` logs `Failed to scan [<jar>] … zip file is
empty` for byte-buddy + the three bouncycastle jars (72× each) and a few others.
**Status on CratonVM:** INVESTIGATED, NOT FIXED. **HotSpot:** PASS.
**Run date:** 2026-06-13 (tag `loop4`).

## Symptom

```
WARN [org.apache.tomcat.util.scan.StandardJarScanner] Failed to scan
  [file:/.../byte-buddy-1.18.8.jar] from classloader hierarchy
  (java/util/zip/ZipException: zip file is empty)
```

Direct probes (`.tooling/JarProbe.java`, `.tooling/ZipProbe.java`) on CratonVM:

| call | CratonVM | HotSpot |
|------|----------|---------|
| `new JarFile(jar).getManifest()` | **works** (non-null) | works |
| `new JarFile(jar).size()` | **0** | 3134 / 6490 / 1529 |
| `new ZipFile(jar).entries()` | **null** → `NPE hasMoreElements` | enumeration |

Note the inconsistency: on the **same** open archive, `getManifest()`/`getEntry`
succeed (they build a name index from `state.archive.file_names()` and find
entries), yet `size()` (`state.archive.len()`) returns 0 and `entries()` (loops
`0..archive.len()`) yields an empty list. So `file_names()` enumerates entries
while `len()` reports 0 — or the handle resolves differently between the two
native paths.

## Where it is

`native-io/src/zip_real_jar.rs` — the zip-crate-backed `JarFile`/`ZipFile`
natives. `native_jarfile_size` returns `state.archive.len() as i32`;
`native_jarfile_entries` iterates `0..state.archive.len()`. Both hinge on
`ZipArchive::len()`, which is returning 0 here even though `getManifest`'s
`file_names()` walk over the same `state.archive` finds entries. The handle
plumbing (`set_jar_handle`/`get_jar_handle` via the `jzfile` field + slot 1) is
the other suspect — `getManifest` and `size` both call `get_jar_handle` but only
one behaves, which would require a per-call handle divergence.

(There is *also* a separate real-JDK zip path — the actual ZipException message
"zip file is empty" is the JDK's, thrown when the Tomcat scanner opens the jar
via a route that does **not** go through these natives — i.e. real
`ZipFile.open0`. So two zip stacks are involved.)

## Why it was deprioritized

The functional jar surface works: `getEntry` / `getInputStream` / `getManifest`
all succeed, so **class loading from these jars is fine** and the suite's tests
run. Only `size()`/`entries()` enumeration is broken, which makes
`StandardJarScanner` skip *annotation* scanning of the affected jars and log a
WARN — non-fatal for essentially all of the 63 classes (they fail/pass for
other reasons). Fixing it cleanly needs runtime handle-tracing (print the
handle + `archive.len()` vs `file_names().count()` in `size`/`entries`/
`getManifest`) to decide between "switch size/entries to the proven
`file_names()` enumeration" and "fix the handle round-trip" — worth doing, but
low pass/fail yield.

## Reproduction

```
cratonvm.exe -cp .tooling JarProbe <byte-buddy.jar> <derby.jar>
# CratonVM: size=0 manifest=true   |  HotSpot: size=3134/1529 manifest=true
```
