# `java.nio.file` symbolic-link probes

Three standalone probes for the symlink surface of `java.nio.file`. All are written to be run
**under both VMs and diffed line-for-line** — every check prints a stable, self-describing line,
so a CratonVM run that matches a real-JDK run of the same source is the pass criterion.

| Probe | Covers |
|---|---|
| `SymlinkProbe.java` | create / read a file and a directory link, relative targets, reading through a link, `FileAlreadyExistsException` on an existing link, `NotLinkException` on a non-link, hard links, delete-link-keeps-target, broken-link semantics, `NOFOLLOW`/follow attribute reads |
| `SymlinkWalkProbe.java` | the follow-vs-don't-follow decisions: `walk`/`find` with and without `FOLLOW_LINKS` over a Kubernetes ConfigMap tree, `walkFileTree` on a symlink root, deleting a link-to-directory, every `LinkOption`-sensitive predicate, and a symlink cycle |
| `SymlinkErrShape.java` | the **shape of the failure** rather than whether it succeeds: `getFile`/`getOtherFile`/`getReason`/`getMessage` of the exception each operation throws, with non-symlink controls (`newByteChannel` on a missing file, `createDirectory` on an existing one, `delete` of a missing file) so any difference can be localized. The only probe here that stays useful on a host that cannot create symlinks at all — see below. |

## Running

```bash
javac SymlinkProbe.java SymlinkWalkProbe.java SymlinkErrShape.java
java SymlinkProbe            > jdk-basic.txt
java SymlinkWalkProbe        > jdk-walk.txt
java SymlinkErrShape         > jdk-shape.txt
cratonvm --cp . SymlinkProbe > cv-basic.txt
cratonvm --cp . SymlinkWalkProbe > cv-walk.txt
cratonvm --cp . SymlinkErrShape  > cv-shape.txt
diff jdk-basic.txt cv-basic.txt && diff jdk-walk.txt cv-walk.txt && diff jdk-shape.txt cv-shape.txt
```

## Host requirement

**Creating a symbolic link on Windows needs `SeCreateSymbolicLinkPrivilege` (an elevated token) or
Developer Mode.** Without either, `Files.createSymbolicLink` fails on **stock HotSpot too** — so a
Windows run of the two functional probes tells you nothing about the VM. Run those on Linux, or on
a Windows host with Developer Mode enabled.

That is also why the Spring Boot classes these probes stand in for
(`ConfigTreePropertySourceTests`, `ApplicationTempTests`, `FileWatcherTests`) fail identically on
both VMs on the Windows suite host — see `apps/spring-boot-suite-runner/RESULTS-20260717.md` §3.
Those three pass 23/23, 6/6 and 15/15 under CratonVM on Linux
(`craton-fullsuite-azure-20260802`), which is what establishes that the Windows rows are a host gap
rather than a masked VM defect.

### Do not match on the message text

Earlier revisions of this file quoted the failure as
`FileSystemException: ... A required privilege is not held by the client`. **That English sentence
is not what you will see.** The text comes from the Win32 error table via `FormatMessage`, so it
arrives in the host's display language — on the current Windows suite host it is Russian, and
`-Duser.language=en` does not change it. The JUnit summary line that reaches a suite log carries no
message at all, only `=> java.nio.file.FileSystemException`.

Match the exception **type** and that structural signature instead, and pair it with a direct
capability probe rather than trusting any string.

### A host without the privilege is still worth a run

`SymlinkErrShape.java` diffs what the *refusal* looks like, so it does its job precisely where the
other two probes are vacuous. That is how the `getReason() == null` defect was found on 2026-08-09:
`FileSystemException` has no `reason` field — the reason lives in `Throwable.detailMessage` — and
the native was writing to a field that does not exist, so the write silently no-opped and every
`FileSystemException` reached Java with the OS's explanation stripped out. It had been sitting
under two "not a CratonVM bug" triage docs. See
`configtree-applicationtemp-windows-symlink-privilege-RETIRED-20260809.md`.
