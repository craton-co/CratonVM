# `java.nio.file` exception shape, and the host gap that kept hiding it

**Status: FIXED 2026-08-11** (branch `fix/sb-four-class-residuals-20260811`).

Closes the two open items left by
`configtree-applicationtemp-windows-symlink-privilege-RETIRED-20260809.md`,
which recorded a diagnosis and a harness design as landed on a branch that in
fact carried no code at all — its own 2026-08-10 reconciliation section says so
("§4's native fix is not present on `dev`… §5's runner-side `ENV-GATED`
reclassification is also not present on `dev`"). Both are here, plus three
further divergences that the same measurement turned up and nobody had looked
for.

## The four classes, and where the line actually falls

Windows reruns across three collectors reported the same four Spring Boot
classes failing, collector-independently. They are two different problems.

| Class | Windows | Linux (CratonVM, 2026-08-11) | Verdict |
|---|---|---|---|
| `org.springframework.boot.autoconfigure.ssl.FileWatcherTests` | FAIL 5/15 | **PASS 15/15** | host gap |
| `org.springframework.boot.env.ConfigTreePropertySourceTests` | FAIL 3/23 | **PASS 23/23** | host gap |
| `org.springframework.boot.system.ApplicationTempTests` | FAIL aborted=1 | **PASS 7/7** | host gap |
| `…ldap.autoconfigure.embedded.EmbeddedLdapAutoConfigurationTests` | FAIL 1/17 | **PASS 18/18** | CratonVM gap, Windows-only |

The Linux runs report `skipped=0 aborted=0`, so the symlink tests genuinely ran
rather than being skipped into a vacuous green.

The first three need `Files.createSymbolicLink`, which on Windows needs
`SeCreateSymbolicLinkPrivilege` (an elevated token) or Developer Mode. This host
has neither, **and stock HotSpot fails the same calls here today** — re-measured
2026-08-11 with Temurin 25.0.3 rather than cited from history:

```
createSymbolicLink | java.nio.file.FileSystemException | file=…\link.txt | reason=<localized OS text>
```

The fourth is a real CratonVM gap and is covered by
`known-issues/springboot/!springboot-ldap-dsa-tls-windows-only-gap.md`, updated
here with the two measurements it previously inferred.

## 1. The harness now classifies the host gap instead of triaging it

The identical `FAIL 23/3` for `ConfigTreePropertySourceTests` appears in at
least ten separate runs between 2026-07-27 and 2026-08-01. A triage loop that
re-derives the same non-answer ten times is the actual defect.

`run-spring-boot-suite.ps1` gains:

* **`Test-HostSymlinkSupport`** — creates a symbolic link in a temp directory
  and reports whether it worked, once per run, and *says the result out loud*.
  A capability probe needs no reference VM, which matters because
  `Resolve-BothFailStatus` cannot help here at all: it requires a same-scope
  HotSpot baseline, and no Windows full-suite HotSpot run has ever existed.
* **`Resolve-EnvGatedStatus`** — records such a row as `ENV-GATED` rather than
  `FAIL`, but only when every failing or aborted test in the class carries the
  symlink signature *and the counts match*. If `failed=3` and only two failure
  blocks are symlink failures, the third is ours and the row stays `FAIL`.

Two deliberate properties:

* **It matches the exception TYPE, never the OS's words.** The Win32 message is
  localized — on this host it arrives in Russian, and `-Duser.language=en` does
  not change it, because it comes from `FormatMessage` rather than from Java. A
  detector keyed on "A required privilege is not held by the client" would
  silently never have fired.
* **Absent evidence never excuses a row.** With no `SBRUNNER_ABORTED_DETAIL`
  line printed, an `aborted=1` cannot match its counter and stays `FAIL`.

`tests/Test-EnvGatedStatus.ps1` covers ten cases, seven of them negatives — a
mixed class, a missing-evidence class, a container failure, a CRASH, and the
same evidence on a host that *can* make symlinks (where these rows are real).
It lifts the functions out of the runner by AST so it cannot drift from them.

Verified against the real Windows logs produced for this branch:
`FileWatcherTests` (failed=5), `ConfigTreePropertySourceTests` (failed=3) and
`ApplicationTempTests` (aborted=1) all classify `ENV-GATED`; the probe reports
`host symlink support: NO (creating a symbolic link failed: Administrator
privilege required for this operation.)`.

## 2. `SbRunner.class` was a month older than `SbRunner.java`

The retired doc could not explain why `SBRUNNER_ABORTED_DETAIL` was landed and
yet appeared in no run log. The reason is that the fixture's compiled runner
predated it — on **both** hosts:

```
Windows fixture:  SbRunner.class Jul 11 17:35   SbRunner.java Aug 10 05:38
Azure fixture:    no SbRunner$OutcomeReasonCollector.class at all
```

The runner only ever checked that `SbRunner.class` *existed*, and the fixture
directory is not under version control, so it goes stale by default. It now
refuses to start when a `.java` is newer than its `.class`, rather than produce
logs missing the very evidence the classifier counts. Recompiling the Windows
fixture immediately produced:

```
SBRUNNER_ABORTED_DETAIL whenSymlinkExistsInDirectoryLocationGetDirThrows() : org.opentest4j.TestAbortedException: Symlink creation not supported
SBRUNNER_SKIPPED_DETAIL whenDirectoryExistsWithWrongPermissionsGetDirThrows() : Disabled on operating system: Windows 11
```

## 3. What was hiding underneath: four exception-shape divergences

Diffing the *shape* of the exceptions rather than whether the call succeeds —
`NioExcShape.java`, run under both VMs on one Windows host, JDK 25 — found four,
of which only two had ever been described.

### 3a. Every `java.nio.file` exception named a forward-slash path

```
HotSpot   msg=C:\Users\…\missing.txt   file=C:\Users\…\missing.txt
CratonVM  msg=C:/Users/…/missing.txt   file=C:/Users/…/missing.txt
```

`Path.toString()` was already correct on both; the divergence was in what the
native handed the exception builder. It affected `newByteChannel`,
`createDirectory`, `delete`, `readSymbolicLink` and `createSymbolicLink`
equally, so it is VM-wide rather than symlink-specific, and it breaks any test
that compares an exception message against a `Path`.

Fixed once, in `p57_exception_path_string`, which every builder now goes through
— rather than at five call sites where a sixth builder would forget. Virtual
filesystem paths (jar and runtime-image entries) are exempt: entry names are
`/`-separated on every platform, exactly as the JDK's own zipfs and jrtfs report
them, and rewriting them would corrupt the encoding as well.

### 3b. `FileSystemException`'s reason was written to a field that does not exist

`java.nio.file.FileSystemException` declares `file` and `other` and nothing
else; its constructor hands the reason to `super(reason)`, and `getReason()`
returns `Throwable.getMessage()`. Measured on the host JDK 25:

```
declaredFields: serialVersionUID, file, other          (no `reason`)
new FileSystemException("F","O","R") -> getReason()=R    getMessage()="F -> O: R"   detailMessage=R
new FileSystemException("F")         -> getReason()=null getMessage()="F"           detailMessage=null
```

`p57_filesystem_exception` was calling `set_field_by_name(exc, "reason", …)`,
which resolves nothing and silently does nothing. Every `FileSystemException`
CratonVM raised therefore reached Java with a null reason and a message
truncated to `"<file> -> <other>"` — including the one exception whose text
identifies this host gap on sight. It now writes `detailMessage`, where
`getReason()` reads. The one-argument builders correctly leave it null; HotSpot
reports `getReason() == null` there too.

### 3c. `Files.delete` of a non-empty directory raised the wrong type

New here, and not symlink-related at all:

```
HotSpot   java.nio.file.DirectoryNotEmptyException
CratonVM  java.nio.file.FileSystemException
```

`p57_delete_error` carried a comment saying the subclass was not modelled and
that "the reason string carries the distinction". It does not: a recursive
delete descends into children exactly when it catches
`DirectoryNotEmptyException`, and a `catch` of a subclass never matches an
instance of its supertype, so that branch silently never ran. There is now a
`p57_directory_not_empty` builder, selected on the raw OS code (`ENOTEMPTY` /
`ERROR_DIR_NOT_EMPTY`) because Rust reports the condition as
`ErrorKind::Uncategorized`, which is what made the distinction unavailable in
the first place.

### 3d. `createSymbolicLink` named two paths where the JDK names one

```
HotSpot   msg=…\link.txt: <reason>                    other=null
CratonVM  msg=…/link.txt -> …/exists.txt              other=…/exists.txt
```

Both JDK providers funnel a failed `createSymbolicLink` through
`rethrowAsIOException(link)` — one path. `createLink` is the genuine two-path
case (`rethrowAsIOException(link, existing)`) and is unchanged.

### 3e. The Windows privilege reason was hardcoded English

`p57_link_io_error` special-cased `ERROR_PRIVILEGE_NOT_HELD` (1314) to the
literal string "A required privilege is not held by the client". The JDK formats
that message through the Win32 error table, so on a non-English host it arrives
localized — which is exactly the host where this error is routine. Rust's
`io::Error` Display already goes through `FormatMessage`, so *deleting* the
special case is what makes the two agree.

## Tests

* `native-builtins`, `exception_shape_tests` (5): the separator conversion and
  its VFS exemption, ENOTEMPTY recognition, and the negative — a missing path
  must not be mistaken for a non-empty directory, or every
  `NoSuchFileException` would come back as the wrong type.
* `native-builtins`, `t27_tls::tests`: `pkcs8_algorithm_name` on RSA/EC/Ed25519
  and on the actual 335-byte key from Spring Boot's LDAP test keystore, plus the
  two declines (not-DER, truncated). `classify_key_type_from_pkcs8` in
  `x509_manager` now delegates to it — a structural walk to the
  `AlgorithmIdentifier` OID rather than a substring scan that can also match key
  material — keeping its own "RSA" last-resort default and its existing tests.
* `apps/spring-boot-suite-runner/tests/Test-EnvGatedStatus.ps1` (10).

## Related

- `configtree-applicationtemp-windows-symlink-privilege-RETIRED-20260809.md` —
  the investigation this completes.
- `files-createsymboliclink-unsupported-FIXED.md` — the 2026-08-01 fix that made
  these calls real.
- `springboot/filewatcher-watchservice-surface-FIXED-20260801.md`
- `known-issues/springboot/!springboot-ldap-dsa-tls-windows-only-gap.md`
