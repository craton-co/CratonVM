# jarmode-tools: four "CratonVM failures" that were a CRLF fixture, and the runner gap that hid it

**Status: FIXED and RETIRED 2026-08-04.**

Four `loader/spring-boot-jarmode-tools` classes were carried as CratonVM
defects in `docs/known-issues/springboot/non-passed.md` and in the
`residual-azure-20260802-32` list. **None of them was a CratonVM defect.**
HotSpot failed all four identically — same test methods, same counts:

| Class | HotSpot (before) | CratonVM (before) | Both (after) |
|---|---|---|---|
| `HelpCommandTests` | FAIL 2/2 | FAIL 2/2 | **PASS 2/2** |
| `ListCommandTests` | FAIL 1/1 | FAIL 1/1 | **PASS 1/1** |
| `ListLayersCommandTests` | FAIL 1/2 | FAIL 1/2 | **PASS 2/2** |
| `ToolsJarModeTests` | FAIL 8/9 | FAIL 8/9 | **PASS 9/9** |

The two sibling classes fixed earlier the same day (`ExtractCommandTests`,
`ExtractLayersCommandTests` — see
[`jarmode-tools-extract-timestamp-preservation-FIXED.md`](jarmode-tools-extract-timestamp-preservation-FIXED.md))
stay green, so **all six `jarmode-tools` classes now pass on HotSpot, on
CratonVM with JIT, and on CratonVM `--nojit`** (runs
`jm2-hotspot-lf-20260804`, `jm2-craton-lf-20260804`,
`jm2-craton-lf-nojit-20260804`). Normalizing the fixture revealed no CratonVM
divergence hiding underneath: the moment the expected files were LF, CratonVM
matched HotSpot on every one of the 9+1+2+2+22+6 tests.

## Root cause

Every failing method is a `TestPrintStream.hasSameContentAsResource(...)`
comparison against a checked-in expected-output `.txt`. The mechanism, taken
byte-for-byte out of a **HotSpot** log (so it cannot be a CratonVM artifact):

```
Expecting actual's toString() to return:
  0000: 55 73 61 67 65 3a 0d 0a  ...   "Usage:" CR LF     <- expected resource
but was:
  0000: 55 73 61 67 65 3a 0a     ...   "Usage:" LF        <- PrintStream.println
```

The expected-output resources are **CRLF**; `PrintStream.println()` on a Linux
host emits **LF**. The assertion message renders the two strings identically,
which is why every previous triage round recorded only
"`AssertionFailedError:` (no message)" and moved on.

This is not upstream Spring Boot. The fixture checkout
`/data/data/springboot-jsonreader-deprecation-20260718` is a single squashed
`fixture_snapshot` commit with **no remote**, and

```
$ git ls-files --eol | awk '{print $1}' | sort | uniq -c | sort -rn
  12788 i/crlf
   1157 i/mixed
    569 i/lf
```

— i.e. the tree was staged from a Windows copy and the CRLF went into the git
blobs themselves. See [[reference_scp_windows_worktree_source_carries_crlf]].

## The two fixes

### 1. The fixture — whole tree (2026-08-04, second pass)

Initially scoped to the 16 `jarmode-tools` resources (below). The tree was then
normalized **in full**, measured before and after:

```
cd <fixture>
git config core.autocrlf input
git add --renormalize .            # 13,945 files
git commit -m "normalize line endings to LF (whole tree)"
git ls-files -z | xargs -0 rm -f   # a plain `checkout --`/`checkout-index -f`
git checkout-index -a -f           # will NOT rewrite them -- see below
```

Restore point: commit `0163c69f` (`fixture_snapshot`) still holds every
original CRLF blob, so any single file can be put back for an A/B.

Two traps worth writing down:

* **`git checkout-index -a -f` does not rewrite the working tree here.** Under
  `autocrlf=input` a CRLF working file over an LF blob already reads as clean,
  so git has nothing to do. The files must be deleted first. `git status` then
  reports ~13.9k `M` entries that are pure stale-stat-cache — `git diff
  --numstat` returns **zero lines**, i.e. no real content difference.
* **The tests read `build/resources/**`, not `src/**/resources`.** Normalizing
  the sources alone changes nothing until the built copies are fixed too: 825
  of them were CRLF. They were stripped in place rather than re-copied from
  `src`, so whatever gradle's `processResources` had produced (including any
  filtering) is preserved and only the line endings change.

#### Measured effect: one class, zero casualties

A full HotSpot run of all 1,976 suite classes before and after, diffed
class-by-class (`hsfull-before-20260804-s{1..6}` vs
`hsfull-after-20260804-s{1..6}`):

| Class | Before | After |
|---|---|---|
| `ChangelogWriterTests` | FAIL 1/1 | **PASS 0/1** |
| `JettyServletWebServerFactoryTests` | PASS 0/113 | FAIL 1/113 |

**Every other one of the 1,975 compared rows is unchanged.** The Jetty row is
load noise, not a normalization casualty: the after-arm ran at load average
104–164 (the standing rule is that reds above ~80 are not trustworthy), the
failing method is `whenServerIsShuttingDownGracefullyThenNewConnectionsCannotBeMade`
— a graceful-shutdown timing test that reads no fixture resource — and it
passes cleanly when re-run alone at load 14.

So the whole-tree CRLF defect was worth **exactly one more class** beyond the
four `jarmode-tools` ones. `ChangelogWriterTests` also passes on CratonVM after
the change. That is the honest scale: 13,945 files carried CRLF, but only five
classes ever compared a changed file against generated text.

The run also produced something the tree did not have before: a **full 1,976-row
HotSpot baseline** (`.suite/baseline/hotspot-baseline-full-lf-20260804.tsv`,
also installed as `hotspot-baseline-latest.tsv`), which is what makes the
`BOTH-FAIL` classification below useful across the whole suite rather than for
whichever handful of classes was last run under `-Vm hotspot`.

### 1b. The fixture — first pass (16 files, `jarmode-tools` only)

`core.autocrlf=input` + `git add --renormalize` + delete-and-restore on
`loader/spring-boot-jarmode-tools/src/test/resources/org/springframework/boot/jarmode/tools/`,
then the same LF copies pushed into `build/resources/test/...` (which is what
is actually on the test classpath; a plain `git checkout --` will NOT rewrite
the working tree, because under `autocrlf=input` a CRLF working file over an LF
blob already reads as clean).

Originals backed up at `/data/data/jm2-crlf-backup/`. Superseded by the
whole-tree pass above, which subsumes it.

### 2. The runner (`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`)

The reason these four reached a known-issues doc at all: a `-Vm hotspot` run
already wrote `<work>/baseline/hotspot-baseline-latest.tsv`, but **nothing ever
read it back**. A class the reference VM failed identically was still recorded
as a plain CratonVM `FAIL`.

The runner now loads that baseline (default path, or explicit
`-HotspotBaseline`) and records `BOTH-FAIL` instead. The rule is deliberately
conservative — see `Resolve-BothFailStatus`:

* only a CratonVM `FAIL` is eligible; a `CRASH`/`HANG`/`LOADFAIL` is
  categorically worse than an assertion failure and is never excused by one;
* the baseline row must itself be `FAIL`, for the same reason in reverse;
* CratonVM must not fail **more** tests than HotSpot did — 5 failures against
  HotSpot's 2 means three are ours, and the row keeps its `FAIL`.

Every non-PASS row gets `hotspot-baseline: <status> <failed>/<tests>` appended
to its note whether or not it was reclassified, so a near-miss is visible
rather than silently dropped. The loader logs the row count, because a baseline
that silently loaded zero rows is indistinguishable from no baseline and would
quietly restore the exact misattribution this exists to prevent.

## Verifying the runner change

Not "it passed" — each branch was driven with an **injected** baseline, all
against the same class (`ChangelogWriterTests`, which fails 1/1 on both VMs),
so no scenario could pass vacuously:

| Baseline row for the class | Recorded | Branch exercised |
|---|---|---|
| `FAIL 1/1` (= CratonVM's 1) | **BOTH-FAIL** | accept |
| `CRASH 1/1` | `FAIL` | refuse: baseline is not FAIL |
| *(absent)* | `FAIL` | refuse: no baseline row |
| `FAIL 0/1` (< CratonVM's 1) | `FAIL` | refuse: CratonVM fails more |

and the notes column carried `hotspot-baseline: CRASH 1/1` / `FAIL 0/1` on the
two refusals that had a row, confirming the refusal is recorded, not silent.

Two earlier attempts at the count and missing-row branches used
`HikariDataSourceConfigurationTests`, `ApplicationPidTests` and
`NoSuchMethodFailureAnalyzerTests` — all three turned out to **pass** on the
current dev tip, so those runs proved nothing and were redone. Worth
remembering: the residual list is stale enough that picking a "known failing"
class without re-checking produces a vacuous green.

## Blast radius of the misattribution

A HotSpot control over the whole `residual-azure-20260802-32.tsv` list found
**5 of 32** classes failing on HotSpot too — the four above plus
`ChangelogWriterTests`. The remaining 27 behave as advertised: where CratonVM
fails and HotSpot passes, the defect is genuine and belongs to its own doc.

The subsequent full-suite HotSpot run put a ceiling on it: across **all 1,976
classes**, those five were the *only* fixture-caused failures, and all five are
now green. The before-arm — with the four `jarmode-tools` classes already fixed
but the rest of the tree still CRLF — recorded 1,931 PASS, 43 `EMPTY` and a
single FAIL (`ChangelogWriterTests`). The 43 `EMPTY` rows are abstract base
classes with no `@Test` methods, not failures. After normalization the counts
are identical except that `ChangelogWriterTests` moved into the PASS column
(the after-arm's one FAIL is the Jetty timing flake described above).

## Reproduce

```bash
ssh -i ~/.ssh/azure.pem victor@20.83.144.174
cd /data/data/cratonvm
# HotSpot control first -- it writes the baseline the craton run then consumes
/snap/bin/pwsh -NoProfile -File ./apps/spring-boot-suite-runner/run-spring-boot-suite.ps1 \
  -Vm hotspot -JdkHome /data/jdk25-real-20260717/jdk-25.0.3+9 \
  -SpringBootRoot /data/data/springboot-jsonreader-deprecation-20260718 \
  -ClassList apps/spring-boot-suite-runner/.suite/jarmodets-20260804.tsv \
  -Parallel 2 -TimeoutSec 900 -RunName <name>-hotspot
```

Then the same command with `-Vm craton -Exe <cratonvm>`; rows HotSpot also
fails now read `BOTH-FAIL`.

## Affected classes

| Module | Class |
|---|---|
| `loader/spring-boot-jarmode-tools` | `org.springframework.boot.jarmode.tools.HelpCommandTests` |
| `loader/spring-boot-jarmode-tools` | `org.springframework.boot.jarmode.tools.ListCommandTests` |
| `loader/spring-boot-jarmode-tools` | `org.springframework.boot.jarmode.tools.ListLayersCommandTests` |
| `loader/spring-boot-jarmode-tools` | `org.springframework.boot.jarmode.tools.ToolsJarModeTests` |
