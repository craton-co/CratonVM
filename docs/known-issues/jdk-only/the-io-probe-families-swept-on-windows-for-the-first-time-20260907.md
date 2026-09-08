# The IO probe families, swept in three arms on Windows for the first time — one fix, one probe that could not run, and four defects it was hiding

**Status: PARTLY FIXED, PARTLY OPEN — MEASURED 2026-09-07.** Windows 11, JDK 25
(Temurin `25.0.3+9`, the same image as the oracle), release binary built from
`origin/dev` at `ca74d321e`. Two fixes landed with this record; four defects are
recorded OPEN with their measurements and nothing else.

**Lane** `claude/jdk-only-finish-20260905`, following
`the-windows-strict-corpus-had-no-baseline-and-two-defects-were-hiding-behind-that-20260905.md`,
which minted the `25-windows` key and turned the blocking strict-corpus step on
for the Windows leg.

---

## 1. Why sweep these six

`scripts/jdk-only-strict-probes.sh` runs a `PROBE_LIST` of **three** probes.
`apps/probes/` holds **117**, and the script's own header records that nothing
else schedules any of them (`grep -c 'probes/' regression-suite/run.sh` is 0).
So 114 probes are compiled by nothing and run by nothing.

`docs/jdk-only-migration.md`'s stage-3 gate names "Windows filesystem/process/
networking vectors stable", so the six filesystem/IO-shaped probes are the ones
whose three-arm behaviour on Windows the rollout actually depends on:

```text
FilePathSweep  FilesSweep  IoSystemSweep  L4FileSweep  L4FilesSweep  AsyncChannelSweep
```

Each was run through the same harness the gate uses — HotSpot control,
`--real-jdk`, `--jdk-only` — which is the only instrument that separates "this
VM is wrong" from "strict mode is wrong".

## 2. The first reading

```text
probe               hotspot   real-jdk   jdk-only   verdict
FilePathSweep         0          0          0       byte-identical, both modes
FilesSweep            0          0          0       DIVERGED in both modes
IoSystemSweep         0          0          0       byte-identical, both modes
L4FileSweep           0          0          0       DIVERGED in both modes
L4FilesSweep          1  <---    0          0       the ORACLE failed
AsyncChannelSweep     0          0          0       byte-identical, both modes
```

**Read the `hotspot` column before the diff.** `L4FilesSweep`'s oracle exited 1,
which the G4 guard reports as "the HotSpot oracle run FAILED, so the expected
side of the diff is an artefact of its failure" — the trap
`read-which-side-of-a-cross-vm-diff-failed-before-reading-the-diff` exists for.
Nothing about that probe's diff meant anything until the oracle ran.

## 3. FIXED — `File.setExecutable(false)` reported success on Windows

```text
                      HotSpot   CratonVM (both modes)
setExecutable true      true      true
setExecutable false     false     true      <-- the defect
```

`apps/probes/L4FileSweep.java`, one row of 480.

**The cause is a sibling that did not delegate.** In
`native-builtins/src/phases_late/nio_file.rs`, `setReadable(Z)`,
`setReadable(ZZ)`, `setWritable(Z)`, `setWritable(ZZ)` and `setExecutable(ZZ)`
all call `fs_set_permission`. `setExecutable(Z)` — alone — hand-rolled its own
body, and was wrong on both platforms:

* **on Windows** it answered `std::fs::metadata(path).is_ok()`, i.e. *"does this
  file exist?"*, so a disable reported `true`. `fs_set_permission`'s Windows arm
  already had the right answer eleven thousand lines away: Windows cannot revoke
  execute, so the JDK reports the request back — `enable`;
* **on Unix** it toggled `0o111`, all three execute bits, where the one-argument
  form is *defined* as the two-argument form with `ownerOnly = true` and should
  touch `0o100` only. `fs_set_permission` already draws that distinction.

**This is the exact shape the block's own comment says was removed.** That
comment — thirty lines above the defect — describes three methods that "answered
'does this file exist?' and changed nothing", calls it "the worst shape
available: a program restricting access to a file believes it did", and names
the probe that caught them. The one-argument `setExecutable` was the fourth and
was missed, and it survived because the sweep that found the others never asked
the **disable** direction on Windows.

Fixed by deleting the hand-rolled body and delegating. `L4FileSweep` is now
**byte-identical to HotSpot in both modes**, 480 rows.

## 4. FIXED — a probe that could not run on Windows at all, and took 350 rows with it

`L4FilesSweep.pathText()` feeds a fixed list of path specs to `Paths.get`. One
of them is `"//"`, which is a hostname-less UNC path:

```text
Exception in thread "main" java.nio.file.InvalidPathException: UNC path is missing hostname: //
    at java.base/sun.nio.fs.WindowsPathParser.parse(WindowsPathParser.java:96)
    at L4FilesSweep.pathText(L4FilesSweep.java:119)
    at L4FilesSweep.main(L4FilesSweep.java:515)
```

Legal on Linux, fatal on Windows — **on the HotSpot arm**, at the fifth spec of
the first section. So `existence`, `readWrite`, `directories`, `copyMove`,
`attributes` and every later section never ran on Windows in any arm, and the
gate scored the probe as an incomplete arm rather than as a comparison.

Fixed in the probe, not the VM: the spec loop reports an `InvalidPathException`
as a row and continues. All three arms print the same row on the same platform,
which is what keeps it a comparison. HotSpot went from dying at row 36 to
**389 rows, rc=0**.

## 5. OPEN — the four defects that probe was hiding

With the probe able to finish, the strict arm diverges from HotSpot on four
rows. **All four are mode-independent** — `--real-jdk` shows them too — so they
are compatibility defects that `--jdk-only` made reachable, not strict-mode
defects. None is fixed here; each is recorded with its measurement.

| # | row | HotSpot | CratonVM |
|---|---|---|---|
| 1 | `Paths.get("//")` | `InvalidPathException` | returns `\\` |
| 2 | `Files.readAllBytes(dir)` | `AccessDeniedException` | `java.io.IOException` |
| 3 | `Files.copy(p, p)` (self) | `true` | `false` |
| 4 | `Files.newByteChannel(dir, WRITE)` | `AccessDeniedException` | `java.io.IOException` |

**#2 and #4 are one root cause, and it is a fix that landed at the wrong level.**
`nio_file.rs` already maps `PermissionDenied` → `AccessDeniedException`,
`NotFound` → `NoSuchFileException` and `IsADirectory` → `FileSystemException`,
with a comment naming the Spring test that motivated it — but it does so **at
`newOutputStream`'s call site only**. `readAllBytes` and `newByteChannel` are
sibling doors onto the same failure and still answer the bare supertype, so a
caller cannot tell "you named a directory" from "the disk is full" — which is
the very sentence that comment uses to justify the mapping. The mapping belongs
below all three doors, not inside one.

**#1 is a path-parser gap**, and it is the one that hid the other three: this
VM accepts `//` where the Windows parser rejects it.

**#3 is unexplained.** `Files.copy` onto the same path answers `true` on HotSpot
and `false` here; I did not shrink it further and have no mechanism to offer.

## 6. What is now known-clean on Windows

`FilePathSweep`, `IoSystemSweep` and `AsyncChannelSweep` are **byte-identical to
HotSpot in both modes**, and `L4FileSweep` joins them after §3. That is four
probe families, several thousand rows, with no divergence at all on a platform
where none of them had ever been run in three arms.

> **SUPERSEDED 2026-09-08 — the limit below was real and is now gone.** The
> paragraph is kept verbatim because the *reason* it gives is the design
> constraint that got fixed: the probe list was shared by every matrix leg, so
> a probe measured on one platform could only be promoted by measuring it on
> all of them. `scripts/jdk-only-strict-probes.sh` now reads a
> `scripts/baselines/jdk-only-strict-corpus-<feature>-<os>.probes` file keyed
> exactly like the baseline it is scored against, so a promotion is per-key and
> a leg with no such file runs the same three probes it always did. **Three of
> the four families named above are in the `25-windows` file as of today**, with
> 60 more from the separate 107-probe sweep and `L4FileSweep` -- 64 in all. (Two
> further sweep probes passed the same bar and were dropped on cost alone: they
> spent 1000s of a 1784s run checking eight rows between them.)
> `L4FilesSweep`, which §3 unblocked by guarding `Paths.get("//")`, is NOT among
> them: re-running it a second time found it diverging in BOTH modes over ten
> sections, which is how it got its own record rather than a gate row.
> **The Linux half of the sentence still stands**: nothing here promotes them
> on `25-linux`, and the next lane with a Linux box still has to measure them
> there before adding that key's file.

**They are NOT added to the gate's `PROBE_LIST` here, and the reason is a
limit rather than a preference.** The default list is shared by every matrix
leg; the gate is a ratchet keyed `<feature>-<os>`; and a probe that diverges on
**Linux** would add keys the `25-linux` baseline does not carry and turn that
leg red for every lane. I have no Linux host, so I can measure exactly half of
what promoting them requires. Naming them here as measured-clean-on-Windows is
what I can honestly support; the next lane with a Linux box can run the same six
and promote whatever is clean on both.

## 7. What this does NOT claim

* **Six probes, not 117.** The other 111 remain compiled by nothing and run by
  nothing on either platform.
* **JDK 25 only.** Both JDK 21 legs of the CI matrix still refuse for want of a
  `21-*` key, and this host carries no JDK 21 image to mint one with.
* **§5's four are recorded, not diagnosed.** Only #2/#4's shared root cause is
  identified, and even that is a reading of the code rather than a measured fix.
