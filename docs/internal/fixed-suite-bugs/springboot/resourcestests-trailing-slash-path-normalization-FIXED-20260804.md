# ResourcesTests: trailing-separator path normalization not applied to `java.nio.file.Path`

**Status: FIXED 2026-08-04** — branch
`fix/nio-path-trailing-separator-20260804`, merged to `dev`. Filed 2026-07-18
(as "RESOLVED", which it never was), re-confirmed failing 2026-07-23,
2026-07-28 (both Windows) and 2026-08-04 (Azure Linux). Moved out of
`docs/known-issues/` only after the failing test was re-run green on the
patched binary against a back-to-back baseline run of the same 8 classes on the
same host.

## Symptom

`org.springframework.boot.testsupport.classpath.resources.ResourcesTests`
`.whenAddDirectoryAndResourceAlreadyExistsThenIllegalStateExceptionIsThrown`
failed in its *setup* call, not its assertion:

```java
void whenAddDirectoryAndResourceAlreadyExistsThenIllegalStateExceptionIsThrown() {
    this.resources.addResource("one/two/three/", "content", true);   // <-- threw here
    assertThatIllegalStateException().isThrownBy(() -> this.resources.addDirectory("one/two/three"));
}
```

`Resources.addResource` (`Resources.java:103-118`) does
`Path resourcePath = this.root.resolve(name)` — `name` still carries the
trailing `/` — then `Files.createDirectories(resourcePath.getParent())` and
`Files.writeString(resourcePath, ...)`. Same failure on both platforms, each
with its own errno:

```
=> java.lang.IllegalStateException: IOException: Is a directory (os error 21)          [Linux]
=> java.lang.IllegalStateException: IOException: <ERROR_DIRECTORY> (os error 267)      [Windows]
   org.springframework.boot.testsupport.classpath.resources.Resources.addResource(Resources.java:114)
```

The trailing separator was never stripped, so `getParent()` returned
`…/one/two/three`, `createDirectories` made **`three` a directory**, and the
write then targeted a directory.

## Root cause

`p57_alloc_path` (`native-builtins/src/phases_late/nio_file.rs`) is the single
allocator every synthetic `Path` funnels through — `Paths.get`, `Path.of`,
`resolve`, `resolveSibling`, `getParent`, `normalize`, `toAbsolutePath`,
`relativize`, `subpath`, `getName`, `iterator`, the walkers. It normalized the
string it stores **only on Windows**:

```rust
#[cfg(windows)]
let stored = if jarfs_decode(path).is_some() {
    path.to_string()
} else {
    p57_trim_path_trailing_separator(path).replace('\\', "/")
};
#[cfg(not(windows))]
let stored = path.to_string();          // <-- the whole defect
```

The JDK normalizes at construction on both: `sun.nio.fs.UnixPath`'s constructor
stores the result of `normalizeAndCheck` — runs of `/` collapse to one
separator, a redundant trailing `/` is dropped, the root `/` keeps its own.
That is exactly `UnixFileSystem.normalize`, which this file already implements
as `file_normalise_path` for `java.io.File`.

`Path.toString()` **hid** the un-normalized string, because it renders through
`file_normalise_path` — so every log line and every assertion on
`path.toString()` looked correct. Only consumers of the raw stored string saw
the defect: `equals`, `hashCode`, `compareTo`, `endsWith`, and the file-IO
bridge that actually syscalls with it.

## Why the earlier attempts missed it

* **2026-07-18 (`8c7f397b0`)** wired `p57_trim_file_trailing_separator` into
  two `java.io.File` methods only (`getAbsolutePath`, `getAbsoluteFile`).
  `Resources.addResource` never touches `java.io.File` — the 07-23 note in the
  old version of this doc diagnosed that correctly.
* **2026-07-30 (`6169b2064`)** added `p57_trim_path_trailing_separator` and
  called it from `p57_alloc_path` — but under `#[cfg(windows)]`, and its unit
  test (`path_construction_trims_only_non_root_trailing_separators`) is
  `#[cfg(windows)]` too, so nothing on Linux ever exercised or checked it. That
  is why the 08-04 Azure run reproduced the *identical* failure with the POSIX
  errno.
* The old doc's "Validation" section (`ResourcesTests`: passed with JIT and
  `--nojit`) could not have been true of any build state containing the code it
  describes. Treated as unreliable and re-run from scratch.

## The reachability trap (cost this session ~an hour)

A `WAVE-4 REACHABILITY CORRECTION` comment inside this same file asserted:

> `register_phase57_nio_file` is reached only from `register_phase57_natives`
> -> `register_synthetic_overrides`, which is
> `#[cfg(feature = "synthetic-jdk")]` and is never called by the default
> real-JDK CLI

**That is false.** `vm/src/vm/vm_init.rs` calls
`cratonvm_native_builtins::phases_late::register_phase57_nio_file` **directly**
in both arms (`vm_init.rs:1788` and `:2273`). This file is the live
`Path`/`Files` implementation in the shipping CLI, and it *overrides*
`native-io/src/lib.rs`'s same-key registrations — which are the ones that look
like the real implementation but never fire.

What *was* synthetic-only is the phase-61 block, `register_p61_files_path`:
`vm_init` never calls it in real-JDK mode. That block re-registered six of
phase 57's `Path` methods, and because registration is last-write-wins it
**displaced** them in synthetic mode — including a `resolve` pair that joined
with `std::path::Path::join` and wrote the result straight into the object,
skipping `p57_alloc_path` and so re-introducing this very defect one phase
later. Those six registrations are deleted on this branch rather than
hand-synced (a `p61_does_not_displace_phase57_path_natives` guard compares the
*winning* registration site for each against the phase-57-only winner, so it
fails if they come back — a presence check would pass just as happily with the
loser left standing).

Settled by instrumenting **both** allocators (`native-io::alloc_path` and
`p57_alloc_path`) with an `eprintln!` and watching which one fired: only the
p57 one did. Static reading had produced two mutually contradictory answers
(`hashCode` behaved like the raw-string native, `toString` like the normalizing
one — they live in different crates). The comment has been corrected in place.

## The second defect, found by the same probe

`PathMatrix.java` (43 `Path` behaviours, diffed line-by-line against the host
JDK 25) showed the `Path` accessors parsing every path with
`p57_parse_win_root` — `sun.nio.fs.WindowsPath` syntax — on **every** target,
and `p57_resolve_paths`/`p57_normalize_path`/`p57_relativize` doing the same.
On Linux that produced:

| expression | HotSpot | CratonVM (before) |
|---|---|---|
| `Paths.get("/tmp").getRoot()` | `/` | `\` |
| `Paths.get("//tmp/x").getRoot()` | `/` | `\\tmp\x\` (a UNC share) |
| `Paths.get("//tmp/x").getNameCount()` | 2 | 0 |
| `Paths.get("C:/x").getNameCount()` | 2 (`C:` is a filename) | 1 (`C:` is a drive) |
| `dir.resolve("a:b")` | `/tmp/base/a:b` | `a:b` (base dropped) |
| `dir.resolve("\foo")` | `/tmp/base/\foo` | `\foo` (base dropped) |
| `Paths.get("a\b").resolve("c")` | `a\b/c` | `a\b\c` |
| `Paths.get("C:a/../b").normalize()` | `b` | `C:b` |
| `Paths.get("a\b/../c").normalize()` | `c` | `a/c` |
| `Paths.get("/a\b").relativize(Paths.get("/a\x"))` | `../a\x` | `../x` |

`a:b` is a perfectly ordinary Unix filename, and `\` is an ordinary Unix
filename character — treating either as path syntax silently dropped the base
of a `resolve` or split one name element into two.

`p57_parse_root` now dispatches to POSIX rules off-Windows (one root `/`, no
drives, no UNC, `\` is a filename character), with a POSIX `getParent` twin
that keeps `.`/`..` name elements verbatim — Rust's
`std::path::Path::parent()` normalizes a trailing `.` away and over-trims
`a/b/.` to `a`, the same trap that motivated the Windows `p57_win_parent_of`.
`p57_resolve_paths` gates the drive/backslash absoluteness test and the join
separator on the target the same way.

## What else the same defect was breaking

The reported test was one symptom. `FilesMatrix.java` (the whole `Files`
surface, trailing-separator arguments, diffed against the host JDK) shows what
the un-normalized stored string cost:

| call | HotSpot | CratonVM (before) | after |
|---|---|---|---|
| `Files.newOutputStream(dir.resolve("d/"))` | writes | `IOException` | writes |
| `Files.copy(c, dir.resolve("e/"))` | copies | `IllegalStateException` | copies |
| `Files.createFile(dir.resolve("f/"))` | creates | `IOException` | creates |
| `Files.move(dir.resolve("f/"), g)` | moves | `IllegalStateException` | moves |
| `Files.newDirectoryStream(dir.resolve("a/b/"))` | `[c, d, e]` | `[c]` | `[c, d, e]` |
| `Files.walk(root).count()` | 6 | 4 | 6 |

`Path.toUri()` additionally never appended the trailing `/` HotSpot appends for
a path that names an **existing directory** (`java.io.File.toURI()` in this
same file already did): `Paths.get("/tmp").toUri()` was `file:///tmp` against
HotSpot's `file:///tmp/`. Before this commit that slash appeared only by
accident, for callers who happened to write one — so removing the accident
without adding the rule would have been a regression. Both `toUri`
registrations now apply it.

## The fix

All in `native-builtins/src/phases_late/nio_file.rs`:

1. `#[cfg(not(windows))] p57_trim_path_trailing_separator` — delegates to
   `file_normalise_path` (collapse `/` runs, drop a redundant trailing `/`,
   keep the root), skipping sentinel-encoded jar/jrt paths exactly as the
   Windows twin does.
2. `p57_alloc_path`'s non-Windows arm calls it. **This is the one-line change
   that fixes the reported test**; everything else is the same layer.
3. `p57_parse_root` (host-syntax root/name parsing) + `p57_posix_parent_of`,
   wired into `getRoot`/`getNameCount`/`getName`/`subpath`/`iterator`/
   `getParent`, and then into `p57_normalize_path`/`p57_relativize` as well.
4. `p57_resolve_paths`: the drive-prefix / leading-backslash absoluteness test
   and the join separator are Windows-only.
5. `Path.toUri()` directory-slash parity, both registrations.
6. The corrected `WAVE-4` reachability comment.
7. `p57_name_elements`: `Paths.get("")` has **one** name element (the empty
   name) on both real `Path` implementations, so `getName(0)`/`subpath(0,1)`
   must not throw and `iterator()` must not be empty. `p57_parse_root` itself
   is deliberately left answering zero elements, because
   `Paths.get("").relativize(Paths.get("a"))` is `a`, not `../a`.
8. `p98_walk_dir` builds the host-filesystem `Path` it hands each
   `FileVisitor` through `p57_alloc_path` instead of writing field 0 directly —
   otherwise a visited path carried host separators and compared unequal to the
   identical path the caller had built. The virtual-FS branches beside it keep
   their sentinel-encoded strings, which `p57_alloc_path` must not touch.
9. The six displaced phase-61 `Path` registrations are deleted, with a guard
   test (see above).

## Validation

All on Azure Linux (`20.83.144.174`), JDK 25 (`/data/jdk25-real-20260717`),
real-JDK mode. Every probe is run identically under `java` and under
`cratonvm` and the two outputs diffed.

* **The doc's own reproducer** (`TrailSep.java`): the `Files.writeString` after
  `root.resolve("one/two/three/")` now succeeds and
  `Files.isRegularFile(root.resolve("one/two/three"))` is true.
* **`PathMatrix`**: 13 divergent lines before, **2** after.
  **`FileMatrix`/`FilesMatrix`/`ResolveMatrix`**: 7 before, **0** after.
  **`NormMatrix`**: 6 before, **1** after. (Re-measured on the final merged
  branch state, not only on the first commit.)
* **Spring Boot `…classpath.resources`, all 8 classes**, same runner, same
  host, back to back:
  * baseline binary (`dev` @ `6b6fc9a0dc`): 7 PASS, **ResourcesTests FAIL**;
  * patched binary: **8 PASS** with JIT, and **8 PASS** again with `--nojit`.
  * The two base commits differ by 12 commits touching `SimpleDateFormat`/JIT/
    tzdb only — nothing in the nio layer — so the A/B attribution holds.
* **Regression sweep, 717 classes** (`cli`, `configuration-metadata`, `core`,
  `loader`, `test-support` — the whole resource/classpath/jar-loading surface),
  patched binary, compared against the 2026-08-02 Azure full-suite baseline.
  See "Regression sweep" below.
* **`cargo test -p cratonvm-native-builtins --lib`**: 3259 passed, 0 failed.
* **Injected-regression check**: reverting just `p57_alloc_path`'s new call and
  re-running made the new `alloc_path_stores_the_normalized_string` test fail
  with `left: "/tmp/one/two/three/"` / `right: "/tmp/one/two/three"`. That test
  allocates through `p57_alloc_path` on a `MockNativeContext` and reads the
  stored field back, so it pins the **wiring**; the helper-only tests would
  have stayed green through the original defect.

New unit tests (`p57_posix_path_tests`, `#[cfg(not(windows))]`, the twin of
`p57_win_path_tests`): `path_construction_normalizes_like_unixpath`,
`parse_root_uses_posix_syntax`, `parent_keeps_curdir_and_root_boundary`,
`alloc_path_stores_the_normalized_string`, plus POSIX counterparts of the
`p57_normalize_relativize_tests` pair (the pre-existing ones now carry
`#[cfg(windows)]`, since their expectations are Windows syntax).

## Regression sweep

Two sweeps, both on Azure Linux against the 2026-08-02 Azure full-suite
baseline (`.suite/results/craton-fullsuite-azure-20260802`). The host was
shared with another session throughout (load average 12–55), which is why every
red below was re-checked individually rather than read off the tally.

**Sweep 1 — 717 classes** (`cli`, `configuration-metadata`, `core`, `loader`,
`test-support`), first patched binary, `-Parallel 4 -TimeoutSec 600`
(`.suite/results/trailsep-regress-20260804`). 12 reds, every one accounted for:

| Red | Verdict |
|---|---|
| 6 × `loader/spring-boot-jarmode-tools` | FAIL in the 08-02 baseline too — pre-existing |
| `ChangelogWriterTests`, `ApplicationPidTests` | FAIL in the 08-02 baseline too — pre-existing |
| `OriginTrackedYamlLoaderTests`, `SimpleAsyncTaskSchedulerBuilderTests` | re-run back to back on both binaries: baseline FAIL/PASS, patched PASS/PASS — flaky, and *better* here |
| `NestedJarFileTests`, `SecurityInfoTests` | FAIL on the **baseline** binary in the same back-to-back run — `dev` drift since 08-02, not from this branch |
| `ImagePackagerTests`, `RepackagerTests` | baseline FAILs in ~7s, patched HANGs — see below |

**Sweep 2 — 144 classes** (`cli`, `configuration-metadata`, `loader`,
`test-support`: the whole jar/classpath/resource surface, i.e. where a `Path`
change bites), final merged branch state, `-Parallel 3 -TimeoutSec 420`
(`.suite/results/trailsep-regress144`): **128 PASS, 10 EMPTY, 3 FAIL, 3 HANG**.

* All **6 `jarmode-tools` classes now PASS** (they were FAIL in sweep 1 and in
  the 08-02 baseline). Credit is `origin/dev`'s
  `fix/jarmode-tools-extract-timestamps-20260804`, merged into this branch —
  noted so the next reader does not attribute it here.
* `ChangelogWriterTests`, `NestedJarFileTests`, `SecurityInfoTests`: the same
  pre-existing FAILs as sweep 1.
* `ImagePackagerTests`, `RepackagerTests`, `ZipContentTests`: HANG at the
  timeout — the JIT-only cluster below.

A second full 717-class sweep on the final binary was started and **abandoned
at 29/717**: a parallel session pushed the host to load 45+, which starved the
run to ~1 class/minute and would have invalidated every red anyway. Sweep 2 was
run instead, scoped to the surface this change actually touches.

### The FAIL → HANG shift, and why it is not this fix

`ImagePackagerTests`/`RepackagerTests` stop failing in seconds and instead burn
the whole per-class timeout (reproduced at 200s/240s/250s/420s ceilings, 5/5
runs). Bisected within this branch — reverting `Path.toUri` did not change it,
reverting the accessor/`parse_root` work did not change it, and the essential
`p57_alloc_path` normalization alone reproduces it — and then explained:

**all four classes pass with `--nojit`, on an unpatched `dev` binary as well.**

| class | `dev` baseline, JIT | `dev` baseline, `--nojit` |
|---|---|---|
| `ImagePackagerTests` | FAIL 5.4s | **PASS 9.1s** |
| `RepackagerTests` | FAIL 20.1s | **PASS 38.6s** |
| `NestedJarFileTests` | FAIL 4.2s | **PASS 23.2s** |
| `ZipContentTests` | CRASH 210.9s | **PASS 143.2s** (final binary) |
| `OriginTrackedYamlLoaderTests` | FAIL 53.8s | **PASS 385.2s** |

So the defect is a JIT miscompile that corrupts what `java.util.zip` reads;
this fix only lets those tests reach further into it before the corruption
shows, and the state they reach there happens to be an unexitable drain loop
rather than a throw. Filed with the full evidence — stack dumps, probes proving
the input bytes are identical to HotSpot's, and the two candidate `Inflater`
fixes that did *not* resolve it — as
[`docs/known-issues/springboot/loader-zip-jit-only-failure-cluster-20260804.md`](../../../known-issues/springboot/loader-zip-jit-only-failure-cluster-20260804.md).

## Known divergences deliberately left in place

All three predate this work, are unrelated to trailing separators, and are the
only probe lines still differing from HotSpot:

* `Path.getFileName()` on a **host** root returns a non-null `""` instead of
  `null`. Deliberate: smallrye-config 3.16's
  `AbstractLocationConfigSourceLoader$ConfigSourceClassPathConsumer.accept`
  calls `.toString()` on the result unguarded and NPEs Keycloak 26 boot. The
  virtual-FS (jar/jrt) root *does* return `null`, which is what javac's
  `ArchiveContainer.preVisitDirectory` needs. See the comment at the
  `getFileName` registration.
* `Path.relativize` between an absolute and a relative path returns the target
  instead of throwing `IllegalArgumentException`. The helper already answers
  `None` for it; only the caller's fallback differs from the JDK contract.
  Changing it would turn a silent wrong answer into a throw in code paths no
  current test exercises, so it was left alone and is recorded here instead.

## Repro artifacts

`~/trailsep/` on the Azure host: `TrailSep.java` (the doc's reproducer),
`PathMatrix.java`, `FileMatrix.java`, `FilesMatrix.java`, `ResolveMatrix.java`,
`NormMatrix.java`, plus `WhoAmI.java`/`Reflect1.java`, which established that
the object is a synthetic `java/nio/file/Path` displayed under the
`sun.nio.fs.UnixPath` alias with the path string in slot 0.
