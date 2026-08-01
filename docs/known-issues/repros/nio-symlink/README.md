# `java.nio.file` symbolic-link probes

Two standalone probes for the symlink surface of `java.nio.file`. Both are written to be run
**under both VMs and diffed line-for-line** — every check prints a stable, self-describing line,
so a CratonVM run that matches a real-JDK run of the same source is the pass criterion.

| Probe | Covers |
|---|---|
| `SymlinkProbe.java` | create / read a file and a directory link, relative targets, reading through a link, `FileAlreadyExistsException` on an existing link, `NotLinkException` on a non-link, hard links, delete-link-keeps-target, broken-link semantics, `NOFOLLOW`/follow attribute reads |
| `SymlinkWalkProbe.java` | the follow-vs-don't-follow decisions: `walk`/`find` with and without `FOLLOW_LINKS` over a Kubernetes ConfigMap tree, `walkFileTree` on a symlink root, deleting a link-to-directory, every `LinkOption`-sensitive predicate, and a symlink cycle |

## Running

```bash
javac SymlinkProbe.java SymlinkWalkProbe.java
java SymlinkProbe            > jdk-basic.txt
java SymlinkWalkProbe        > jdk-walk.txt
cratonvm --cp . SymlinkProbe > cv-basic.txt
cratonvm --cp . SymlinkWalkProbe > cv-walk.txt
diff jdk-basic.txt cv-basic.txt && diff jdk-walk.txt cv-walk.txt
```

## Host requirement

**Creating a symbolic link on Windows needs `SeCreateSymbolicLinkPrivilege` (an elevated token) or
Developer Mode.** Without either, `Files.createSymbolicLink` fails with
`FileSystemException: ... A required privilege is not held by the client` on **stock HotSpot too** —
so a Windows run without those tells you nothing about the VM. Run these on Linux, or on a Windows
host with Developer Mode enabled.

That is also why the Spring Boot classes these probes stand in for
(`ConfigTreePropertySourceTests`, `ApplicationTempTests`, `FileWatcherTests`) fail identically on
both VMs on the Windows suite host — see `apps/spring-boot-suite-runner/RESULTS-20260717.md` §3.
