# `Files.createSymbolicLink` always threw `UnsupportedOperationException` — no real symlink creation

**Status: FIXED — 2026-08-01** (branch `fix/nio-symlink-create-20260801`)

Supersedes `docs/known-issues/springboot/files-createsymboliclink-unsupported-20260731.md`.

## Symptom (as filed 2026-07-31)

`org.springframework.boot.env.ConfigTreePropertySourceTests` — 3/23 failures, all identical shape:

```
java.lang.UnsupportedOperationException
   java.nio.file.spi.FileSystemProvider.createSymbolicLink(FileSystemProvider.java:626)
   java.nio.file.Files.createSymbolicLink(Files.java:976)
   org.springframework.boot.env.ConfigTreePropertySourceTests.createSymbolicLink(...)
```

plus 5 of the 14 `FileWatcherTests` failures, and the `Assumptions.abort` path in
`ApplicationTempTests.whenSymlinkExistsInDirectoryLocationGetDirThrows`.

## Root cause (confirmed — the filed diagnosis was correct)

`Files.createSymbolicLink(Path, Path, FileAttribute...)` is ordinary JDK bytecode:
`provider(link).createSymbolicLink(link, target, attrs); return link;`. CratonVM's default
`FileSystemProvider` is a synthetic instance stamped as the literal
`java/nio/file/spi/FileSystemProvider` class, so that invokevirtual lands on the base class's
**own concrete body** — an unconditional `throw new UnsupportedOperationException()`. Same for
`createLink` and `readSymbolicLink`.

Three natives *were* registered for these names, but on `java/nio/file/Files` — the static wrapper
class, not the receiver — and `Files.createSymbolicLink` has real bytecode of its own, which wins.
They were dead, and they also lied: each returned the link path without creating anything.

Confirmed by probe against the pre-fix binary: `createSymbolicLink`, `createLink` and
`readSymbolicLink` all threw `UnsupportedOperationException` from
`FileSystemProvider.java:717` (JDK 21 line numbering), never reaching a native.

## Repairs

1. **Real link natives on the provider.** `createSymbolicLink`/`createLink`/`readSymbolicLink`
   registered on `java/nio/file/spi/FileSystemProvider`, backed by `std::os::*::fs::symlink*`,
   `std::fs::hard_link` and `std::fs::read_link`. Relative targets are stored verbatim (a
   Kubernetes ConfigMap tree is relative links into a hidden `..data` directory). On Windows the
   file-vs-directory link flag is chosen by resolving the target against the link's parent, as
   `WindowsLinkSupport` does.

2. **Admitted past the concrete-bytecode-wins rule.** New shared predicate
   `is_file_system_provider_link_native_override` (`vm/src/runtime/interpreter/invoke.rs`), wired
   into *both* dispatch gates — `force_native_over_real_jdk_bytecode` and vm_exec's
   `check_override` — plus the base-registration routing in
   `intercept_force_registered_native`. Exactly the shape `newFileChannel` already needed, and for
   exactly the same reason. Guarded by
   `file_system_provider_link_ops_force_native_over_the_base_class_throw` in
   `vm/src/runtime/interpreter.rs`.

3. **JDK exception types for link failures.** `FileAlreadyExistsException`, `NoSuchFileException`,
   `AccessDeniedException`, a new typed `NotLinkException`, and a new typed `FileSystemException`
   carrying the `(file, other, reason)` triple. That last one is what Windows reports without
   Developer Mode or an elevated token — "A required privilege is not held by the client" — which
   is a *host* limitation HotSpot reports identically, not a missing feature.

4. **The three `java/nio/file/Files` registrations made real** rather than deleted, so any dispatch
   path that does prefer a registration gets the same answer as the provider.
   `Files.readSymbolicLink` also gained a host-filesystem path; it previously handled only jrt
   package links and answered `NoSuchFileException` for every real symbolic link.

### Follow-on repairs the fix exposed

Symbolic links could not previously exist under CratonVM, so every link-blind corner of
`java.nio.file` was unreachable. Creating them made all of these live at once:

- **The file-tree walkers ignored `FileVisitOption` entirely** and descended into every directory
  symlink via `Path::is_dir()`. The JDK reads each entry `NOFOLLOW` unless `FOLLOW_LINKS` is given,
  so a link is a *file* to the walk. `walkFileTree`/`walk`/`find` now thread the option through and
  decide dir-ness off the readdir record's own file type.
- **`walkFileTree` treated a non-directory root as a directory.** The JDK reports it through a
  single `visitFile`. Together with the point above, this is what made
  `FileSystemUtils.deleteRecursively(symlink)` recurse into the link's *target* and then fail to
  unlink the link — the stranded link then poisoned every later test in `ApplicationTempTests`.
- **`Files.delete`/`deleteIfExists` chose `remove_dir` vs `remove_file` with `Path::is_dir()`**, so
  a link to a directory took `remove_dir` and failed with ENOTDIR; `deleteIfExists` also read a
  dangling link as absent.
- **`Files.exists`/`notExists`/`isDirectory`/`isRegularFile` discarded their `LinkOption[]`**: a
  dangling link "did not exist" and a link to a directory "was a directory" even under
  `NOFOLLOW_LINKS`.
- **`Files.find` passed no `LinkOption` to `readAttributes`**, so a `BiPredicate` could never see
  `attrs.isSymbolicLink()` on a non-following walk.

The whole `WatchService` implementation was also repaired — a *different* filed bug
(`filewatcher-watchservice-timed-poll-missing-native-20260731.md`), but its dead watcher thread
made the five symlink-dependent `FileWatcherTests` cases unverifiable, so it is fixed in the same
branch. See `filewatcher-watchservice-timed-poll-missing-native-FIXED.md`.

## Verification

Linux (`victor@20.83.144.174`, Ubuntu, JDK 21) — a host where symbolic links can actually be
created, unlike the Windows suite host, which lacks `SeCreateSymbolicLinkPrivilege` and where
*both* VMs fail these classes identically (see `apps/spring-boot-suite-runner/RESULTS-20260717.md`
§3). Binary `cratonvm-symlink-20260801` (release, `f1649b0f49`).

Two probes, each diffed line-for-line against a real-JDK run of the same source — the pass
criterion is byte-identical output, not "no exception":

- `SymlinkProbe` — 20 checks: create/read file and directory links, relative targets, read through
  a link, `FileAlreadyExistsException` on an existing link, `NotLinkException` on a non-link, hard
  links, delete-link-keeps-target, broken-link semantics, `NOFOLLOW`/follow attribute reads.
  **IDENTICAL to the real JDK.**
- `SymlinkWalkProbe` — 18 lines covering the follow-vs-don't-follow decisions: `walk`/`find` with
  and without `FOLLOW_LINKS` over a Kubernetes ConfigMap tree, `walkFileTree` on a symlink root,
  delete of a link-to-directory, every `LinkOption`-sensitive predicate, and a symlink cycle.
  **IDENTICAL to the real JDK.**

Spring Boot classes, run through `SbRunner` against both VMs on that host:

| Class | HotSpot | CratonVM before | CratonVM after |
|---|---|---|---|
| `ConfigTreePropertySourceTests` | 23/23 | 3 failures (UOE) | **23/23** |
| `ApplicationTempTests` | 6/6 | 3 failures | **6/6** |
| `FileWatcherTests` | 15/15 | 14 failures | **15/15** |

Repeated **3 consecutive times** with identical results — `FileWatcherTests` is timing-dependent
(it waits on real filesystem events), so a single green run would not have settled it.

Probe sources: `docs/known-issues/repros/nio-symlink/`.

### A host-fixture trap worth remembering

`ConfigTreePropertySourceTests` creates `/tmp/symlinkTempDir` at a FIXED path. A run that dies
before its cleanup leaves that symlink behind, and the next run of the class — **on either VM** —
fails with `FileAlreadyExistsException: /tmp/symlinkTempDir`. Seen once mid-investigation and
briefly misread as a regression. `rm -rf /tmp/symlinkTempDir` before rerunning.
