# jarmode-tools: preserve `ExtractCommand` / `ExtractLayersCommand` entry timestamps

**Status: FIXED and RETIRED 2026-08-04.** Both affected classes are green on
CratonVM with JIT and with `--nojit`, on the Azure Linux host where they were
last red:

| Mode | `ExtractCommandTests` | `ExtractLayersCommandTests` |
|---|---|---|
| `all-jit` | **PASS 22/22** (was FAIL 1/22) | **PASS 6/6** (was FAIL 3/6) |
| `all-nojit` | **PASS 22/22** | **PASS 6/6** |

Re-confirmed on the exact tree that was merged to `dev` (`b6f0ecdb2`, which
also carries another session's intervening JIT/x64 work):
`.../jarmodets-merged-20260804/` — both classes PASS.

Runs: `apps/spring-boot-suite-runner/.suite/results/jarmodets-fix1-20260804/`
and `.../jarmodets-fix1-nojit-20260804/`. Binary
`/data/data/jarmodets-probe/cratonvm-jarmodets-fix1-20260804`, built from
`fix/jarmode-tools-extract-timestamps-20260804`, JDK
`/data/jdk25-real-20260717/jdk-25.0.3+9`, Spring Boot root
`/data/data/springboot-jsonreader-deprecation-20260718`.

## What was actually wrong (2026-08-04)

**`BasicFileAttributeView.readAttributes()` reported `1970-01-01T00:00:00Z`
for `lastModifiedTime()`, `lastAccessTime()` and `creationTime()` on every
Unix host, regardless of the file's real timestamps.**

Everything upstream of it was already correct, which is what made the earlier
triage rounds chase the wrong files:

* `setTimes` worked. The inode really did carry the requested time —
  `stat` on the extracted files printed `mtime=2021-01-01 00:00:00.000000000`
  while the same run's `readAttributes()` answered 1970.
* The ZIP read side worked. `JarFile.entries()`, `JarFile.getJarEntry()` and
  `ZipFile.getEntry()` all returned the entry's real `mtime`/`atime`/`ctime`,
  parsed out of the `0x5455` extended-timestamp extra field.
* The ZIP *write* side worked. A jar written by CratonVM's
  `JarOutputStream` is byte-comparable to HotSpot's in its local and central
  extra fields (`5554 0d 00 07 …` local, `5554 05 00 07 …` central, plus the
  `0xCAFE` JAR magic on the first entry).

The defect was a single mismatched pair of field names in the attributes
carrier, in `native-builtins/src/phases_late/nio_file.rs`:

* `basic_file_attributes_store` wrote `st_birthtime`, `st_atime`, `st_mtime`.
* `basic_file_attributes_time_millis` read those same three names back.

The real `sun.nio.fs.UnixFileAttributes` has **never** declared any of them.
It stores each timestamp as a SECONDS + NANOS pair — `st_mtime_sec` /
`st_mtime_nsec`, `st_atime_sec` / `st_atime_nsec`, `st_birthtime_sec` /
`st_birthtime_nsec`, plus a `birthtime_available` flag (confirmed with
`javap -p --module java.base sun.nio.fs.UnixFileAttributes` on Temurin
25.0.3+9).

`set_field_by_name` on a name the class does not declare is a **silent
no-op**, and `get_field_by_name` answers a non-`Long` that the reader mapped
to `0`. So the store and the load agreed *with each other* and with nothing
else: a self-consistent pair that always produced 0 millis. Nothing in the
bridge could ever notice, because no code path compared the two against the
inode. `st_mode` and `st_size` — the two names in the same block that *are*
genuine — kept working, which is why `isDirectory()`/`size()` never hinted at
the problem.

### Why it read green in July and red in August

The 2026-07-18 round was verified **on Windows only** (binary
`C:\craton\cratonvm-jarmode-timestamps-20260718-019f742a.exe`). The Windows
carrier's names — `creationTime`, `lastAccessTime`, `lastWriteTime` on
`sun.nio.fs.WindowsFileAttributes` — are genuine, so the store and load both
landed and Windows was actually correct. The Linux half was never covered.

The 2026-08-04 note's **suspect regression window is wrong**: `968572d757`
("close loader ZIP residual shard", 2026-07-28) reworked `zip_real_jar.rs` and
the `alloc_zip_entry` / `real_layout` / `dos_time` machinery, but none of that
is on this path. This was never a regression — it is a platform gap that the
July verification could not have caught, and the first Linux run of these
classes exposed it.

The suite history confirms that rather than assuming it. Grepping every run
under `apps/spring-boot-suite-runner/.suite/results/` for these classes finds
exactly two pre-fix runs, and both are red with **identical** counts:

| Run | `ExtractCommandTests` | `ExtractLayersCommandTests` |
|---|---|---|
| `craton-fullsuite-azure-20260802` (first Linux run covering them) | FAIL 1/22 | FAIL 3/6 |
| `craton-residual32-20260804` | FAIL 1/22 | FAIL 3/6 |

There is no green Linux run to have regressed from, and no change in the
failure between the 08-02 and 08-04 runs that a 07-28 commit could explain.

## The fix

`native-builtins/src/phases_late/nio_file.rs`:

* `unix_attr_time_fields` maps each logical time to its real
  `(_sec, _nsec)` pair, with the unsplit spelling kept as a third element for
  a synthetic-JDK shim that declares it.
* `basic_file_attributes_time_millis` and the new `unix_attr_store_time`
  resolve the `_sec` field with `resolve_field_index_by_class_id` and use the
  pair when the carrier declares it, falling back to the legacy single field
  otherwise. Neither side can silently address storage the other does not.
* `birthtime_available` and `st_ctime_sec`/`st_ctime_nsec` are populated too,
  so real JDK bytecode reaching `UnixFileAttributes.creationTime()` or
  `ctime()` directly sees what our overrides see instead of 1970.
* The synthetic-stub branch (fixed slot layout) and the Windows branch are
  unchanged.

## Regression gate

`regression-suite/src/RFileTimes.java`, added to `CORE_CLASSES` in
`regression-suite/run.sh`, so a plain `bash regression-suite/run.sh` runs it.
The suite diffs CratonVM's `CK`/`PASS` lines against HotSpot's, in real-JDK
mode (`--java-home`).

It was **proved to catch this defect**, not merely to pass: run against the
pre-fix binary it fails with

```
    --- HotSpot ---
    CK plain.readAttributes.lastModified 2021-01-01T00:00:00Z
    CK plain.readAttributes.lastAccess 2022-01-01T00:00:00Z
    --- CratonVM ---
    CK plain.readAttributes.lastModified 1970-01-01T00:00:00Z
    CK plain.readAttributes.lastAccess 1970-01-01T00:00:00Z
```

and passes against the fixed one. Note that the same run's
`CK plain.Files.getLastModifiedTime` and `CK plain.File.lastModified` lines
are **correct in both arms** — those reach different natives that never went
through the attributes carrier. That is exactly why the failure looked so
narrow, and why the vector prints all three spellings.

Stages 2-4 of the vector replay the jarmode-tools extract pipeline without
Spring on the classpath: write a jar whose entries carry explicit FileTimes,
read them back through `JarFile` and `ZipFile`, extract each entry, push its
time onto the extracted file with `setTimes`, and read it back.

## Collateral-damage check

The attributes carrier is shared, so the whole `residual-azure-20260802-32.tsv`
list was re-run with the fixed binary (`jarmodets-resid32-20260804`) and diffed
row-for-row against the `craton-residual32-20260804` baseline. 32 rows in, 32
rows out; the **only** status changes are the two target classes:

```
< ExtractCommandTests        FAIL 1/22      > ExtractCommandTests        PASS 0/22
< ExtractLayersCommandTests  FAIL 3/6       > ExtractLayersCommandTests  PASS 0/6
```

One further row differs without changing status:
`KafkaAutoConfigurationIntegrationTests` FAIL 0/0 → FAIL 0/3 — a known
hang-class whose run got far enough to report three tests this time. It is
FAIL in both arms and is unrelated to file attributes.

Also green with the fixed binary: the full core regression suite,
**23 passed / 0 failed** (`CV=<fixed> JDK=/data/data/jdk25-real bash
regression-suite/run.sh`), and `cargo check -p cratonvm-native-builtins` on
Windows, since the change is in a file with `#[cfg(windows)]` branches.

## Follow-up round, 2026-08-04: the deferred parity gap, and a second defect

The `ZipEntry` accessor divergence this doc deferred is now **CLOSED**.
Chasing it also surfaced a second, undocumented defect on the sibling path.

1. **`getLastAccessTime()`/`getCreationTime()` diverged from HotSpot.** The
   JDK's `ZipOutputStream` writes the 0x5455 extended-timestamp field **twice
   with different payloads**: the local header carries every time it was given
   (13 bytes, flags `0x07`), the central-directory copy keeps the same flags
   byte but carries the modified time alone (5 bytes). Verbatim from a Temurin
   25.0.3+9 jar: `5554 0d00 07 66ee5f80 99cf6100 cdb06300` local,
   `5554 0500 07 66ee5f80` central. `ZipFile`/`JarFile` read the central
   directory, so HotSpot answers `null` for both; only `ZipInputStream`, which
   walks local headers, sees all three.

   We read the local header on top of the central one and merged it in.
   Removed — `zip_local_entry_times` / `p59_zip_local_entry_times` and their
   byte parsers are gone. The `zip` crate already answers correctly unaided:
   `by_index`/`by_index_raw` both reach `central_header_to_zip_file`, whose
   `ExtendedTimestamp` parser handles the 5-byte central record by ignoring
   the flag bits it has no bytes for. This also removes a `File::open` plus
   two seeks and a read **per entry, per `entries()` call**.

2. **`JarFile.entries()` reported `1979-11-30T00:00:16Z`** for an entry
   carrying only a DOS timestamp — found by the parity probe, not by any test.
   The Spring Boot loader bridge dual-writes synthetic slot 1, which is
   `xdostime` in the real JDK layout, so the entry's **size** landed in the
   timestamp field (size 8 → DOS seconds 16). The later `set_field_by_name`
   writes repair every other clobbered slot; `xdostime` was the one nobody
   wrote back, and only `getLastModifiedTime()` on an entry with no `FileTime`
   ever reads it. Fixed by carrying the central DOS timestamp through
   `JarEntryTimes` and setting `xdostime` by name — the same remedy
   `zip_real_jar.rs` already applied via its `real_layout` gate, whose comment
   records the identical "DOS date in 1979" symptom.

Neither defect was reachable from the tests' own assertions:
`entryTimeAttributes` compares two archives read through the same bridge, so
it stayed green either way.

**Verification** (binary `cratonvm-jmzip2-fix-20260804`, JDK 25.0.3+9, Azure):

| check | result |
|---|---|
| `ZipTimeParity` probe diffed against HotSpot | identical except the jar's own creation second (the two runs wrote their jars 2 s apart) |
| `RFileTimes` (HotSpot-diffed) | PASS |
| full core regression suite | **25 passed / 0 failed** |
| `ExtractCommandTests` | PASS 22/22 on JIT **and** `--nojit` |
| `ExtractLayersCommandTests` | PASS 6/6 on JIT **and** `--nojit` |

`ZipInputStream` still reports all three times, matching HotSpot — the fix is
not an over-correction of the streaming path.

`RFileTimes` gained both accessors for every entry plus a DOS-only entry
(written with `setTime`, which clears `mtime` so `ZipOutputStream` emits no
0x5455) on an even second, since DOS timestamps cannot represent an odd one.
**Limitation worth stating:** the pre-fix binary had already been discarded
when the vector was extended, so unlike the first round's vector this one is
not *demonstrated* to fail against it. What is measured is that the pre-fix
binary answered `2022-01-01`/`2023-01-01` for those accessors and
`1979-11-30T00:00:16Z` for the DOS-only read — the exact quantities the vector
now prints and diffs against HotSpot.

## Deliberate non-changes

* **The four other `loader/spring-boot-jarmode-tools` classes are still red**
  — `HelpCommandTests` (2/2), `ListCommandTests` (1/1),
  `ListLayersCommandTests` (2/1), `ToolsJarModeTests` (9/8). Their
  failed-test counts are **byte-identical before and after this fix**, so the
  change neither helped nor hurt them. They are text-output assertions
  (`TestPrintStream` against an expected resource), a different mechanism, and
  they are tracked in `docs/known-issues/springboot/non-passed.md` awaiting
  their own triage.

## Reproduce

```bash
ssh -i ~/.ssh/azure.pem victor@20.83.144.174
cd /data/data/cratonvm
/snap/bin/pwsh -NoProfile -File ./apps/spring-boot-suite-runner/run-spring-boot-suite.ps1 \
  -Vm craton -Exe <cratonvm> -JdkHome /data/jdk25-real-20260717/jdk-25.0.3+9 \
  -SpringBootRoot /data/data/springboot-jsonreader-deprecation-20260718 \
  -ClassList apps/spring-boot-suite-runner/.suite/jarmodets-20260804.tsv \
  -Parallel 2 -TimeoutSec 900 -RunName <name>
```

Standalone, no Spring Boot checkout needed:

```bash
CV=<cratonvm> JDK=<jdk25> ONLY="RFileTimes" bash regression-suite/run.sh
```

## Original report (2026-07-18 round)

`ExtractLayersCommandTests` creates archive entries with creation, access and
last-modified `FileTime` values, then verifies that extraction preserves the
last-modified value. CratonVM's Spring Boot `JarFile.entries()` bridge cached
only the name, sizes, method, CRC and content bytes, so the synthetic
`JarEntry` had no `ZipEntry.mtime` and `getLastModifiedTime()` fell back to
the archive's DOS timestamp — `1980-01-01` (`315446400` Unix seconds) for
JDK-written entries. The lower-level real ZIP bridge had the same omission,
and `BasicFileAttributeView.setTimes` was a no-op.

That round parsed the `0x5455` and `0x000a` extra fields, materialized
`mtime`/`atime`/`ctime` on both bridges' entries, and implemented
`setTimes` against the host filesystem. All of that is still in place and
still correct — see "What was actually wrong" for why it was not sufficient
on Linux.

## Affected classes

| Module | Class |
|---|---|
| `loader/spring-boot-jarmode-tools` | `org.springframework.boot.jarmode.tools.ExtractLayersCommandTests` |
| `loader/spring-boot-jarmode-tools` | `org.springframework.boot.jarmode.tools.ExtractCommandTests` |
