# `ConfigTreePropertySourceTests` + `ApplicationTempTests` — Windows symlink privilege

**Status: RETIRED — 2026-08-09** (branch `fix/springboot-symlink-host-gap-20260809`).

Supersedes `docs/known-issues/springboot/configtree-applicationtemp-windows-symlink-privilege-20260807.md`.

The retired doc's headline conclusion — *the Windows suite host cannot create
symbolic links, and that is not a CratonVM bug* — is **correct, and is now
verified rather than asserted**. It is retired because the two things it existed
to do are now done by code instead of by prose:

1. the suite runner **detects the host gap itself** and records those rows as
   `ENV-GATED`, so they no longer arrive as `FAIL` needing triage;
2. the test log now **states the reason in words**, so a reader never has to
   reconstruct it from upstream test source.

Retiring it also closed a real CratonVM defect that the original triage sat on
top of without noticing (§4).

---

## 1. The premise, re-verified

The retired doc asserted the host lacks `SeCreateSymbolicLinkPrivilege`. Checked
directly on 2026-08-09:

```
whoami /priv                      -> no SeCreateSymbolicLinkPrivilege in the token
HKLM:\...\AppModelUnlock
  AllowDevelopmentWithoutDevLicense -> unset (Developer Mode off)
```

and confirmed behaviourally against the reference VM, which needs no CratonVM
involvement at all:

```
$ java SymlinkProbe          # Temurin 25.0.3, docs/known-issues/repros/nio-symlink/
[FAIL] createSymbolicLink(file) -> java.nio.file.FileSystemException: ...\link.txt: <localized>
         at java.base/sun.nio.fs.WindowsFileSystemProvider.createSymbolicLink(...)
```

So the premise holds. Both classes fail here on stock HotSpot too.

### The message is OS-localized — do not match on its English text

The retired doc, and
[`docs/known-issues/repros/nio-symlink/README.md`](../../../known-issues/repros/nio-symlink/README.md),
both quote the failure as *"A required privilege is not held by the client"*.
On this host the string arrives in **Russian**, and `-Duser.language=en` does not
change it — the text comes from the Win32 error table via `FormatMessage`, not
from Java. Worse, the JUnit summary line that reaches the suite log carries no
message at all, only `=> java.nio.file.FileSystemException`.

Any detector keyed on that English sentence would therefore have silently never
fired. Match the exception **type** and that structural signature instead.

## 2. What the retired doc got wrong

- **Its run does not exist.** It cites
  `craton-fullsuite-windows-20260806-s1` and log paths beneath it. There is no
  such run directory; the 2026-08-06 Windows runs are
  `craton-nonpassed-20260806-s{1..7}`, and neither class appears in them. The
  `FAIL 23/3` row it describes is real but comes from the earlier runs that do
  carry it — `broad-NEGCTL-20260727`, `jitfix-20260727`, `mockfix2-20260727`,
  `craton-rerun-20260731`, `craton-rerun-20260801`,
  `bothfix-coreregr-20260801`, `devtools-cluster-coreregr-20260801`,
  `r2dbcfix-coreregr-20260801` — **ten runs of the identical row**, which is the
  clearest possible sign the triage loop was not converging.

- **It concluded without exercising the code.** It reasons entirely from test
  source and from the fact that the call cannot succeed here. It never ran
  CratonVM's symlink paths anywhere they *can* succeed, so "not a CratonVM bug"
  was an inference, not a measurement — and §4 shows a CratonVM bug was in fact
  sitting inside that exception.

## 3. The measurement the retired doc was missing

Symlink creation needs no privilege on Linux, and the suite already had Linux
full-suite runs. Under CratonVM on the Azure shard
(`craton-fullsuite-azure-20260802`):

| Class | status | tests | failed | aborted | skipped |
|---|---|---:|---:|---:|---:|
| `org.springframework.boot.env.ConfigTreePropertySourceTests` | PASS | 23 | 0 | 0 | 0 |
| `org.springframework.boot.system.ApplicationTempTests` | PASS | 6 | 0 | 0 | 0 |
| `org.springframework.boot.autoconfigure.ssl.FileWatcherTests` | PASS | 15 | 0 | 0 | 0 |

The totals match the Windows totals (23 and 6), with `skipped=0`/`aborted=0` —
so the symlink tests genuinely **executed** rather than being skipped into a
vacuous green. CratonVM's symlink surface is correct; the Windows rows are a
host gap and nothing was hiding behind it functionally.

## 4. The CratonVM bug that *was* hiding behind it

Diffing the **shape of the exception** (not whether the call succeeds) across
both VMs on this host — `SymlinkErrShape.java`, added to the nio-symlink repro
set — showed CratonVM losing the OS's explanation entirely:

```
HotSpot   getReason  = <the OS's reason text>      getMessage = "<link>: <reason>"
CratonVM  getReason  = null                        getMessage = "<link> -> <target>"
```

Root cause: **`java.nio.file.FileSystemException` has no `reason` field.** The
real class declares only `file` and `other`; the constructor passes the reason to
`super(reason)`, and `getReason()` returns `Throwable.getMessage()`. Confirmed
against the host JDK 25 — `getDeclaredField("reason")` throws
`NoSuchFieldException`, while `new FileSystemException("F","O","R")` reports
`getReason() = R` and `getMessage() = "F -> O: R"`.

`p57_filesystem_exception` was writing `set_field_by_name(exc, "reason", …)`,
which resolves nothing and **silently does nothing** (the known by-name-write
failure mode). Every `FileSystemException` CratonVM raised therefore reached Java
with a null reason and a truncated message — including the one exception whose
text would have identified this host gap on sight. The carefully written
`ERROR_PRIVILEGE_NOT_HELD` reason string in `p57_link_io_error` was being
constructed and then dropped.

Two fixes in `native-builtins/src/phases_late/nio_file.rs`:

- write the reason to `detailMessage`, where `getReason()` reads it;
- `createSymbolicLink` names only the **link** in the exception, as both the
  Windows and Unix JDK providers do (`rethrowAsIOException(link)`), rather than
  link *and* target. `createLink` is the genuine two-path case and keeps both.

`p57_no_such_file` and the other single-path builders correctly leave
`detailMessage` null and are unchanged: HotSpot likewise reports
`getReason() == null` for a one-argument `FileSystemException`.

> **Both land on `fix/filewatcher-symlink-surface-20260809`, not on this
> branch.** That parallel investigation reached the identical diagnosis and the
> identical two edits independently, and additionally fixes the path-separator
> divergence noted at the end of this doc. This branch therefore carries no
> Rust change — the finding is recorded here because it is what retires the
> "not a CratonVM bug" framing, whichever branch ships the code.

## 5. Why this can no longer become a known-issues doc

**Host-gap classification** — the runner now probes the host's symlink
capability directly and records those rows as `ENV-GATED` rather than `FAIL`, so
they arrive already explained instead of needing triage. That lands on
`fix/filewatcher-symlink-surface-20260809` (`Test-HostSymlinkSupport` /
`Resolve-EnvGatedStatus`), which gates per test method. A capability probe needs
no reference VM, which matters because the `BOTH-FAIL` reclassifier cannot help
here at all: it requires a same-scope HotSpot baseline, and no Windows
full-suite HotSpot run has ever existed.

`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`, on this branch:

- **`Resolve-BothFailStatus`** now compares `aborted` and `containersFailed`, not
  just `failed`. It previously excused a row where CratonVM aborted 5 tests and
  HotSpot aborted 1, because `0 -gt 0` is false — the counters that made
  `ApplicationTempTests` a `FAIL` were the ones it was not looking at.
- **`Merge-HotspotBaselineLatest`** stops `hotspot-baseline-latest.tsv` being
  clobbered. Every `-Vm hotspot` run blind-copied its results over it, so
  "latest" meant *most recent*, not *best*: a 5-class check on 2026-08-01
  replaced an 81-class baseline, and the reclassifier has been running against 5
  irrelevant rows since. Rows now merge on `module + class`.

`apps/spring-boot/sb-runner/SbRunner.java` (and `SbRunnerMethod.java`): JUnit's
`SummaryGeneratingListener` counts aborted and skipped tests but carries a reason
for neither, which is why `ApplicationTempTests` reached the log as a bare
`aborted=1` with no explanatory text anywhere. It now prints one line per test:

```
SBRUNNER_ABORTED_DETAIL whenSymlinkExistsInDirectoryLocationGetDirThrows() : org.opentest4j.TestAbortedException: Symlink creation not supported
SBRUNNER_SKIPPED_DETAIL whenDirectoryExistsWithWrongPermissionsGetDirThrows() : Disabled on operating system: Windows 11
```

Byte-identical under both VMs on this host. That is the retired doc's entire
§"`ApplicationTempTests`'s `aborted=1`/`skipped=1` breakdown" section, produced
by the harness instead of by hand.

## 6. Still true, and still not a code problem

Enabling Developer Mode (`Settings → Privacy & Security → For developers`) or
running the suite from an elevated token makes all three classes pass on Windows
too. That remains host configuration, not a VM change. The difference is that
the suite no longer *reports* it as a CratonVM failure while it is absent.

## Related

- `files-createsymboliclink-unsupported-FIXED.md` — the 2026-08-01 fix that made
  these calls real in the first place. This doc is the next layer down: what the
  call throws when the host refuses it.
- `nio-write-ignores-nofollow-links-symlink-20260804-FIXED.md`
- [`docs/known-issues/repros/nio-symlink/README.md`](../../../known-issues/repros/nio-symlink/README.md)
  — dual-VM probes, now including `SymlinkErrShape.java` for the exception shape.

## Adjacent finding, fixed on the FileWatcher branch

Every `java.nio.file` exception CratonVM raises on Windows carried a
**forward-slash** path (`C:/Users/…`) where HotSpot carries the platform form
(`C:\Users\…`). `Path.toString()` itself is identical on both VMs — the
divergence was in what the native passed to the exception builder
(`p57_read_path` hands back the internal representation). It affected
`newByteChannel`, `createDirectory` and `delete` equally, so it is VM-wide
rather than symlink-specific. Fixed on
`fix/filewatcher-symlink-surface-20260809` via a `p57_exception_path` helper
applied at each exception builder; because that changes the text of every nio
exception message on Windows, the next full-suite run is worth watching for
tests that compare such a message against a `Path`.
