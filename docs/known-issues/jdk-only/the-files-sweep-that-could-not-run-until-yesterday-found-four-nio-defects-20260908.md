# The `Files` sweep that could not run until yesterday found four `java.nio.file` defects, all mode-independent

**Status: OPEN — MEASURED 2026-09-08.** Windows 11, Temurin `25.0.3+9` (the
same image as the oracle), release binary built from `dev` @ `2b0913374`. Three
arms: HotSpot control, `--real-jdk`, `--jdk-only`. **`--real-jdk` and
`--jdk-only` transcripts are byte-identical to each other**, so none of this is
a strict-mode question. No fix is proposed here.

**Reads with**
`the-io-probe-families-swept-on-windows-for-the-first-time-20260907.md`, whose
§3 is the reason this page can exist at all.

---

## 1. Why nobody had seen these

`L4FilesSweep` used to die on the **oracle** arm. Its `pathText()` loop calls
`Paths.get(s)` over a spec list, and spec 5 is `"//"` — on Windows a UNC path
with no hostname, which HotSpot rejects with `InvalidPathException`. The probe
did not catch it, so the HotSpot arm exited at row 36 of 389 and the harness
correctly refused to score anything: *an oracle that fails is not an oracle.*

§3 of the 09-07 record guarded that call, HotSpot went to 389 rows and `rc=0`,
and the probe became measurable for the first time. **This page is what it then
measured.** The guard was written to keep the oracle alive; the first thing the
living oracle did was disagree with us in four places.

## 2. The four

Ten divergent sections, eighteen differing rows, identical under both modes:

| # | row | HotSpot | CratonVM (both modes) |
|---|---|---|---|
| 1 | `Paths.get("//")` | `THREW java.nio.file.InvalidPathException` | accepted: `toString=\`, `root=\`, `nameCount=0`, `isAbsolute=false` |
| 2 | `Files.readAllBytes(<dir>)` | `THREW java.nio.file.AccessDeniedException` | `THREW java.io.IOException` |
| 3 | `Files.copy(p, p)` — self-copy | `true` | `false` |
| 4 | `Files.newByteChannel(<dir>, WRITE)` | `THREW java.nio.file.AccessDeniedException` | `THREW java.io.IOException` |

#1 is eight of the eighteen rows on its own: once the path is accepted, the
eight follow-up probes on it answer where HotSpot has already thrown.

## 3. #2 and #4 share a discriminator, and it is in the transcript

The tempting reading of #2/#4 is "we do not map `ERROR_ACCESS_DENIED`". The
transcript refutes it: the rows either side of #2 are

```text
 newInputStream dir     |THREW java.nio.file.AccessDeniedException|
 newBufferedReader dir  |THREW java.nio.file.AccessDeniedException|
```

— the same directory, the same underlying open, the **right** exception. So the
mapping exists and works; what #2 and #4 have is a path that reaches the
generic `IOException` before the mapping is applied. `readAllBytes` and
`newByteChannel(WRITE)` are the two that take it. That is a much smaller thing
to look for than a missing mapping, and it is a single hypothesis covering both
rows — which is exactly why it is written down as a hypothesis and not a
diagnosis.

## 4. What this does NOT claim

* **Nothing is diagnosed to a call site.** §3 narrows #2/#4 to a shape; it does
  not name the function that throws the wrong type, and #1 and #3 are not
  narrowed at all.
* **#1 is a Windows statement.** `"//"` is a rooted path on POSIX and an
  invalid UNC path on Windows; this run is Windows only, and the probe has
  never been run in three arms on Linux.
* **One probe, one platform, one JDK.** The row counts are `L4FilesSweep`'s,
  not a census of `java.nio.file`.
* **Not a `--jdk-only` defect.** Both modes agree with each other and disagree
  with HotSpot, so retirement is not the instrument here — the same reasoning
  `phase-2s-last-lead-closed-itself-...-20260908.md` §3 sets out at length.
* **The probe is deliberately NOT promoted into the gate.** Its four siblings
  (`AsyncChannelSweep`, `FilePathSweep`, `IoSystemSweep`, `L4FileSweep`) are in
  `scripts/baselines/jdk-only-strict-corpus-25-windows.probes`; this one is a
  lead. Baselining ten divergent sections would freeze these four defects into
  the ratchet as known-acceptable, which is the opposite of the point.
