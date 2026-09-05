# W7-42 — the two instrument holes: the sides were not the same class file, and the streams were not apart

**Status: BOTH ROOT-CAUSED FROM STORED ARTEFACTS AND FIXED IN SOURCE 2026-08-12.**
Branch `fix/differential-instrument-holes-20260812`. The probe change is
verified end to end on HotSpot 25.0.3.9 and on the release binary at
`C:/craton/CratonVM/target/release/cratonvm.exe` (built 2026-08-12 00:21). The
`native-builtins` change is a claim about source; it is not rebuilt here.

> **VERIFIED AGAINST A BINARY 2026-09-02.** The `native-builtins` half was "a
> claim about source; it is not rebuilt here". It has now been run, and the
> instrument this record built did the job it was built for.
>
> ```text
> PROBE-SECTIONS=28
> PROBE-LEDGER=missing:0,undeclared:0,duplicate:0,multiline:0,unrenderable:0
> ```
>
> Emitted identically by HotSpot 25 and by CratonVM `--real-jdk`, both at 872
> lines with an EMPTY diff.
>
> **The ledger is what makes that empty diff mean something.** `W7-36`'s
> differential went 43 -> 0 on this run. A zero from a differential is exactly
> the shape this record exists to distrust — hole 1 was a probe whose two sides
> were not the same class file, so lost lines read as agreement. `missing:0,
> undeclared:0, duplicate:0` is the line that separates "agreed on 28 sections"
> from "stopped early and agreed on nothing", and it is present on both sides.
>
> The nine markers §"RE-VERIFIED IN TREE" enumerates are in the restored probe,
> and `PROBE-LEDGER` / `PROBE-SECTIONS` are the two that fired here.
>
> **Not covered:** the `[SUREFIRE-NPE]` forensic gate is still a source claim —
> this run did not set `surefire_npe_trace_enabled()`, so hole 2's fix is
> unexercised by it.

Predecessor: W7-40-differential-at-14.md (which reported the two holes as two
of its fourteen divergences), W7-33-differential-dead-sections.md (whose
excised scratchpad copy of the probe is hole 1's mechanism),
W7-32-round-2-differential-run.md.

**The differential is at 9, not 14.** Five of the fourteen were the
instrument.

> **RE-VERIFIED IN TREE 2026-08-12, and the pipeline's remaining holes are now
> closed too (§"The pipeline, audited a second time" below).**
>
> Everything this record claims to have fixed is present in the working tree:
> `probes/ShadowDifferentialProbe.java` carries all nine ledger markers
> (`MISSING-OBSERVABLE`, `UNDECLARED-OBSERVABLE`, `SECTION-NEVER-RAN`,
> `SECTION-UNDECLARED`, `DUPLICATE-OBSERVABLE`, `MANIFEST-OVERFLOW`,
> `PROBE-MANIFEST-DIGEST`, `PROBE-LEDGER`, plus `<toString-threw:…>` and the
> explicit `System.out.flush()`), and `native-builtins/src/lib.rs:38324` gates
> the `[SUREFIRE-NPE]` forensic behind `surefire_npe_trace_enabled()` (the
> helper is defined at `native-builtins/src/lib.rs:33`; `git log -S` on the
> symbol returns exactly one commit, `59a7ba47a`). Neither hole is open.
>
> Re-checked 2026-08-12 (second pass): the gate line had drifted from `:38238`
> to `:38324` and the number above has been corrected. The sibling gate this
> record cites is also still true — `vm/src/runtime/exceptions.rs:1941` wraps
> the `SUREFIRE-NPE-TRACE` `eprintln!` in `if dbg_npe_trace()`.
>
> **Three standing rules for anyone re-measuring this**, all of which cost this
> area a lane already:
>
> * **NO `regression-suite/run.sh` RUN CAN RETIRE ANYTHING IN THIS RECORD, AT
>   ANY `SUITE=` VALUE.** Verified 2026-08-12: `run.sh` contains the string
>   `probes` **zero** times. It schedules only `regression-suite/src/R*.java`,
>   from the two explicit lists at `run.sh:106` and `run.sh:119`, dispatched at
>   `run.sh:207-213` (`core` / `jdk-only` / `all`); nothing is scheduled by
>   glob, and `modules/` and `modules-overlay/` each hold exactly one entry
>   (`cratonvm.jdkonly.svc`), neither of them a probe. `ShadowDifferentialProbe`
>   appears nowhere under `regression-suite/` except a provenance comment at
>   `regression-suite/src/RJdkViews.java:29`. The only thing that runs this
>   instrument is `probes/shadow-differential.ps1`, **by hand**.
>   A green `SUITE=all` is not evidence here. Every hole this record names —
>   the ledger markers and the six pipeline items P1–P6 — is evidenced only by
>   something under `probes/`, so the suite is *structurally* incapable of
>   discharging them. The two items with evidence outside `probes/` are the
>   `[SUREFIRE-NPE]` gate and the `Formatter.close` item, and the suite has no
>   vector that trips either.
>
> * **Diff against this record's transcript and its `PROBE-MANIFEST-DIGEST`,
>   never against W7-4's retired oracle.** That oracle predates the manifest
>   ledger, so diffing a current run against it MANUFACTURES divergence out of
>   rows the probe has since gained — RETIREMENT-20260812.md retires it on
>   exactly that ground. No baseline transcript is stored beside the probe, and
>   that is deliberate: a frozen expected-output file beside a probe that keeps
>   growing is the same trap with a filename.
> * **One compile, both sides.** Two builds are two objects and are not
>   comparable. That *is* hole 1, and it is now a property of the script rather
>   than of the operator's discipline.
>
> One correction to the framing, not to the finding: the campaign brief cites
> W7-4's oracle as 858 lines. **858 is this probe's declared-observable count**;
> RETIREMENT-20260812.md records W7-4's oracle as **540** lines. Both numbers are
> in play in this area and they are not the same number.

---

## Hole 1 — five observables vanished while the fence stayed silent

`ArrayDeque.addNull`, `ArrayDeque.addFirstNull`, `ArrayDeque.offerNull`,
`ArrayDeque.sizeAfterRefusedNulls` and `COW.addAllAbsent` printed on HotSpot
and were **absent** from the CratonVM transcript — no line at all, not a line
with a different value — while both enclosing sections ran to completion and
`SECTION-DIED` never appeared.

### The mechanism, measured

**The two sides were not running the same class file.**

W7-33 Part 3 needed the two dead sections to reach the end on an unfixed
binary, so it made a scratchpad copy of the probe with exactly those five
`line(..)` statements excised, and said so. The copy outlived the run. The
next differential compiled HotSpot from the tree and CratonVM from the copy.

This is not inferred from the shape of the output. Both artefacts are still on
disk in the session scratchpad that produced W7-40, and they name themselves:

```text
scratchpad/pbuild/ShadowDifferentialProbe.class   20:06   contains "ArrayDeque.addNull"
scratchpad/probes/ShadowDifferentialProbe.class   22:25   does NOT
scratchpad/probesrc/ShadowDifferentialProbe.java  22:25   diff vs tree = exactly those 5 statements, deleted
scratchpad/hotspot.txt   859 lines   has ArrayDeque.addNull, has COW.addAllAbsent
scratchpad/craton3.txt   861 lines   has neither
```

`diff hotspot.txt craton3.txt`, with line endings normalised, reproduces
**W7-40's published fourteen byte for byte** — including the four `481,484d480`
deletions and the `570d572` deletion. That is the whole of hole 1: five
statements that were not in the source one side executed.

The fence stayed silent because there was nothing to be silent about. Nothing
threw. Nothing failed. The premise of the comparison had lapsed, and no part
of the pipeline was in a position to notice.

### Why the fence could never have caught it

The fence catches a section that **died**. This was a section that
**finished**, correctly, having been asked to do less. A fence is a
throw-detector; absence is not a throw. Everything the probe's own record
claimed about truncation ("a truncated transcript reads exactly like a short
clean run") applies with more force to a transcript that is not truncated at
all and is simply missing five rows in the middle: a reader scanning a diff
for `<`/`>` **pairs** sees agreement.

### The fix: the probe declares what it owes

`probes/ShadowDifferentialProbe.java` no longer relies on control flow
reaching a `line(..)` call.

- `manifest()` declares all **858** observables, by section, in emission
  order. `line(..)` ticks each off. The end of a section prints
  `MISSING-OBSERVABLE=<name>` for each one still owed. **The declaration and
  the statement are separate text** — that is the entire point. Deleting the
  statement, or running a side whose source never had it, leaves the
  declaration behind to accuse it.
- `UNDECLARED-OBSERVABLE=<name>` reports the other direction, so the manifest
  cannot quietly fall behind a newly added row and re-open the hole for it.
- `SECTION-NEVER-RAN=<name>` reports a `section(..)` call deleted from `main`.
- `PROBE-MANIFEST-DIGEST` closes the case the ledger is itself complicit in: a
  statement **and** its declaration deleted on one side. Two sides printing
  different digests were not built from the same probe, and no comparison
  between them means anything.
- `PROBE-SECTIONS`, `PROBE-OBSERVABLES-DECLARED`,
  `PROBE-OBSERVABLES-EMITTED`, `PROBE-LEDGER=missing:…` always print. A
  healthy run's totals diff clean, so they cost one identical line each.

The ledger is built from `String[]`, `boolean[]` and `String.equals` only.
This probe exists to test `java.util`; an instrument that keeps its own
bookkeeping in a `LinkedHashSet` is a variable of the comparison it is
running, and would report a clean run precisely when the collection it leans
on is the broken thing. The digest is plain `long` arithmetic over `charAt`
rendered to hex by hand, for the same reason — no `MessageDigest`, no
`String.hashCode`, no `String.format`.

The manifest was **generated from the source and reconciled against the
859-line HotSpot transcript**, not asserted: 858 declared names, 0
declared-but-never-emitted, 0 emitted-but-never-declared. A manifest nobody
reconciled would be a probe that cannot fail.

---

## Hole 2 — the diagnostic was never on stdout

Seven `[SUREFIRE-NPE]` frames appeared in the CratonVM transcript. They were
read as a leak from the VM's diagnostic stream into the observable stream.

**Measured on the binary: they are on stderr, and always have been.** The
emitter at `native-builtins/src/lib.rs` is `eprintln!`; `git log -S` finds
exactly one commit ever touching that line, and it introduced it as
`eprintln!`. A six-line class that throws `NullPointerException("Name is
null")` puts them on fd 2 with fd 1 clean:

```text
fd1: OBS.before=1 / OBS.valueOfNull=java.lang.NullPointerException / OBS.direct=Name is null / OBS.after=1
fd2: [SUREFIRE-NPE] Name is null thrown; top Java frames: …
```

So the leak was in the **capture**: the transcript was taken with the streams
merged. Same species as hole 1 — the defect is in how the differential was
run, and it was read as a property of the VM.

Two changes.

**The capture.** `probes/shadow-differential.ps1` separates the streams at the
OS level (`Start-Process -RedirectStandardOutput/-RedirectStandardError`, not
an inline redirect: PowerShell 5.1 wraps a native executable's stderr in an
ErrorRecord). Only stdout is diffed. stderr is written beside it and **not
discarded** — it is where the VM says why, and it is where the one genuinely
new finding below turned up.

**The emitter.** The forensic is now opt-in behind the existing
`CRATONVM_DBG_NPE_TRACE` (`CRATONVM_DBG=npe-trace`), the flag
`vm/src/runtime/exceptions.rs` already gates its sibling NPE-origin dumps
with. No new flag, so no `types/src/flag_groups.rs`,
`types/tests/flag-surface.txt`, `docs/flag-tokens.md` or
`docs/config/flag-inventory.md` churn. It was unconditional and fires for
**any** `NullPointerException("Name is null")` — including
`java.lang.Enum.valueOf(null)`, which ordinary application code and every
differential probe reach on purpose — and the Surefire bootstrap
investigation it was cut for is closed.

Nothing about what `Enum.valueOf` throws, or its message, is touched here.

---

## The audit: seven more ways this probe could lose a row silently

The two holes were found by a careful reader noticing absent lines. That is
not a detection method. Asked what else could go missing without a fence
firing, the probe had **seven** further paths, all now closed the same
structural way.

| # | path | was | now |
|---|---|---|---|
| 1 | a `section(..)` call deleted from `main` | every one of its observables gone, nothing thrown | `SECTION-NEVER-RAN` + one `MISSING-OBSERVABLE` each |
| 2 | `main`'s 63-observable inline preamble | **outside any section** — unfenced *and* unledgered, so a throw there truncated the whole transcript with no marker | the `factoriesAndViews` section |
| 3 | a value rendering with an embedded CR/LF | one observable becomes several transcript rows and shifts the alignment of every row after it — exactly the corruption the leaked `[SUREFIRE-NPE]` frames caused | escaped in `line(..)`, counted as `multiline:` |
| 4 | the same key emitted twice | a set-based check cannot notice one of them going away | `DUPLICATE-OBSERVABLE` (zero today, measured) |
| 5 | a `line(..)` added with no declaration | the hole re-opens for that row | `UNDECLARED-OBSERVABLE` |
| 6 | a `toString()` that throws inside `line(..)` | unwinds to the fence, turning ~40 later observables in that section into absence | `<toString-threw:Type>`, a value, and the section continues |
| 7 | stdout not flushed at exit | the tail is absence of exactly the kind this record is about | explicit `System.out.flush()` |

A section running without a manifest entry (`SECTION-UNDECLARED`) and a
manifest larger than the table (`MANIFEST-OVERFLOW`) are reported too.

### Every new check was proven RED before its green was accepted

Faults injected into a copy of the probe and run on HotSpot:

| injected fault | reported as |
|---|---|
| the exact five-statement W7-33 excision | 5 `MISSING-OBSERVABLE`, `EMITTED=853`, `missing:5` |
| `section("dequeEdges", …)` deleted from `main` | `SECTION-NEVER-RAN=dequeEdges` + 30 `MISSING-OBSERVABLE` |
| statement **and** declaration deleted | digest `015b87011871ad91` vs `22732607802c59c2`, `DECLARED=857` |
| a `line(..)` with no manifest entry | `UNDECLARED-OBSERVABLE=ZZ.undeclared section:dequeEdges` |
| a key emitted twice | `DUPLICATE-OBSERVABLE=ArrayDeque.pop` |
| a value containing CR and LF | `ArrayDeque.pop=multi\nline\rvalue`, `multiline:1` |
| a `toString()` that throws | `ArrayDeque.content=<toString-threw:java.lang.IllegalStateException>`, section survives, `missing:0` |

And the instrument change moved no measurement: the first 858 lines of the
instrumented probe's HotSpot transcript are **byte-identical** to the
pre-change one.

---

## The differential, re-taken: 9

One compile, both sides against the same class directory, streams apart, on
the release binary of 2026-08-12 00:21.

```text
hotspot   stdout 864 lines   stderr  0 lines
cratonvm  stdout 864 lines   stderr 15 lines
both: PROBE-LEDGER=missing:0,undeclared:0,duplicate:0,multiline:0,unrenderable:0
both: PROBE-MANIFEST-DIGEST=22732607802c59c2
```

```diff
< stream.reuseThrows=java.lang.IllegalStateException
> stream.reuseThrows=no-throw
< Enum.valueOfBadName=java.lang.IllegalArgumentException: No enum constant ShadowDifferentialProbe.Color.MAUVE
> Enum.valueOfBadName=java.lang.IllegalArgumentException: No enum constant MAUVE
< format.unknownConversion=java.util.UnknownFormatConversionException: Conversion = 'q'
> format.unknownConversion=java.lang.IllegalArgumentException: Conversion = 'q'
< format.missingArgument=java.util.MissingFormatArgumentException
> format.missingArgument=java.lang.IllegalArgumentException
< format.wrongArgumentType=java.util.IllegalFormatConversionException
> format.wrongArgumentType=java.lang.IllegalArgumentException
< format.illegalFlagCombination=java.util.IllegalFormatFlagsException
> format.illegalFlagCombination=java.lang.IllegalArgumentException
< format.precisionOnInteger=java.util.IllegalFormatPrecisionException
> format.precisionOnInteger=java.lang.IllegalArgumentException
< NumberFormat.currencyNegativeUS=-$1,234.50
> NumberFormat.currencyNegativeUS=($1,234.50)
< Random.nextGaussian=1.1419053154730547
> Random.nextGaussian=1.141905315473055
```

All nine are the value divergences W7-40 already adjudicated and none of them
is owned here. The five `ArrayDeque`/`COW` rows are **not** defects: run from
the same class file they match HotSpot exactly, which also converts W7-33's
"claimed, not verified" for the `ArrayDeque` null refusal into a measurement.

```text
ArrayDeque.addNull=java.lang.NullPointerException
ArrayDeque.addFirstNull=java.lang.NullPointerException
ArrayDeque.offerNull=java.lang.NullPointerException
ArrayDeque.sizeAfterRefusedNulls=0
COW.addAllAbsent=1:[a, b, zz, qq]
```

---

## New, for someone else: `Formatter.close()` calls `close()` on a `StringBuilder`, and the `NoSuchMethodError` is swallowed

The stderr sidecar — which a merged, `grep -v`-filtered capture had been
destroying — carries a defect that has never appeared in any transcript.

```java
StringBuilder out = new StringBuilder();
try (java.util.Formatter f = new java.util.Formatter(out, java.util.Locale.ROOT)) {
    f.format("%d", 7);
}
```

HotSpot: nothing. CratonVM, on fd 2:

```text
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError
  method="java/lang/StringBuilder.close()V"
  caller="Cl.main([Ljava/lang/String;)V @pc=159"
```

`pc=159` is the try-with-resources `invokevirtual java/util/Formatter.close()V`,
so the bad invoke is raised inside `Formatter.close()`. The JDK guards it:

> the appendable is closed only when it is an instance of `Closeable`

and `java.lang.StringBuilder` is not. The guard is being taken the wrong way
**inside that method only** — the same test in the caller answers correctly on
both VMs, so this is not a broken `instanceof`:

| | HotSpot | CratonVM |
|---|---|---|
| `sb instanceof Closeable` | `false` | `false` |
| `sb instanceof Flushable` | `false` | `false` |
| `sb instanceof Appendable` | `true` | `true` |
| `Closeable.class.isInstance(sb)` | `false` | `false` |

Two things are wrong and the second is the worse one. The JDK's `catch` on
that path is `IOException`, so on HotSpot a `NoSuchMethodError` there would
**propagate**. Here it is logged as a `WARN` and execution continues with the
right answer (`format.formatterAppendable` matches on both sides, which is why
no observable ever showed it). A linkage error that becomes a log line is a
fabricated success, and it is invisible to every probe in this campaign
because every one of them reads stdout.

Not fixed here — **and the sentence that used to stand here was false.** It
read "`java.util.Formatter` has no `close` registration anywhere in the tree".
It does: `native-builtins/src/lib.rs:41579`, inside `register_formatter_natives`
(`:41537`). That registration predates this record by weeks (`git log -S` on the
register line reaches only the initial commit; the intervening `bcfb51ffd` is a
pure `extract` refactor), so it was never a matter of drift — the claim was
wrong when written, and a reader who greps for it bounces straight off the
record.

What survives the correction is the *underlying* observation, which is a
dispatch-path question rather than a native to patch:
`register_formatter_natives` is the **synthetic-mode** registrar —
`native-builtins/src/lib.rs:21384-21394` says so in terms ("the synthetic-mode
registrar `register_formatter_natives` DOES need the overload", and on the
real-JDK boot path "the class is the REAL `java.util.Formatter`"). So under
`--real-jdk` the JDK's own `Formatter.close()` bytecode runs and takes the
`Closeable` guard the wrong way, which is what the repro shows. Not this
branch's surface. Repro above is six lines and deterministic.

Separately, the native's own swallow is still live and still bare —
`native-builtins/src/lib.rs:41584`, `let _ = ctx.invoke_virtual(target, "close",
"()V", &[]);` — which is W7-57 rows 48–51, confirmed OPEN.

> Line-number drift note for the index: `README.md:281-283` cites the four
> `java.util.Formatter` close/flush sites at `:21440`/`:21455`/`:41498`/`:41506`;
> two of them are now at `:41584` and `:41592`.

---

## The pipeline, audited a second time (2026-08-12)

The probe's Java was the half this record fixed. The **runner** was audited
afterwards and had six more ways to lose or misread a row, all in
`probes/shadow-differential.ps1`, all now closed. Nothing was run: this section
is a source claim, and the command that turns it into a measurement is below.

| # | the hole | why it is the same species | now |
|---|---|---|---|
| 1 | a digest mismatch printed the diff anyway, under a warning | a warning above a plausible-looking diff is a warning that gets scrolled past, and scrolling past *this* condition is what produced hole 1 | **no diff is printed at all**; exit 2, transcripts and provenance left on disk |
| 2 | `PROBE-LEDGER` was matched against a frozen five-field string | the ledger already grew twice (`multiline`, `unrenderable`); a frozen pattern goes red on a **healthy** run the day it grows again, and a check that fires on a healthy tree gets muted rather than read | parsed as `key:value` pairs, every field must be `0`, unparsable and non-numeric fields reported by name |
| 3 | the stderr sidecar was kept and **never read** | this record's one genuinely new finding came from that file and had never appeared in any transcript. Separating the streams and then not reading the sidecar is the same blindness one step further back | scanned for nine linkage/lookup errors (`NoSuchMethodError`, `NoSuchFieldError`, `AbstractMethodError`, `IncompatibleClassChangeError`, `NoClassDefFoundError`, `ClassNotFoundException`, `UnsatisfiedLinkError`, `IllegalAccessError`, `VerifyError`), one regex alternation so a line naming two is reported once, printed **above** the diff |
| 4 | no cross-side check on the emitted count | both sides can report `missing:0` — each is only self-consistent — while having emitted different numbers of rows. That combination is hole 1's exact shape | `PROBE-OBSERVABLES-EMITTED` compared across sides |
| 5 | nothing recorded what produced a transcript | the digest proves the two sides of **one** run agree; hole 1's mechanism was a scratchpad copy **outliving** the run, which no within-run check can see | `provenance.txt` beside the transcripts: probe path + SHA-256, classes dir, `javac -version`, each side's resolved exe path + SHA-256 + mtime + argv + exit code, both digests |
| 6 | `Get-Content` decoded both transcripts in the host ANSI codepage | the sides are pinned to `-Dstdout.encoding=UTF-8`; the currency and text rows this probe exists to adjudicate are exactly the ones that mangle | `-Encoding UTF8` on every read, `@()` so `.Count` is a line count on an empty or one-line file |

Sidecar findings deliberately do **not** suppress the diff and do not change
the exit code: they are findings about the VM, not instrument errors. The
banner says so, and says that a run with sidecar lines and a clean diff is not
a clean run.

### Re-measuring: one command, and the numbers are blank because nothing was run

```powershell
.\probes\shadow-differential.ps1 `
    -Cratonvm .\target\release\cratonvm.exe `
    -Java "C:\jdk-25.0.3.9-hotspot\bin\java.exe" `
    -VmArgs "--real-jdk"
```

It compiles once, runs both sides against that one class directory, and exits
0 / 1 (divergence) / 2 (instrument error). Fill in from its output:

```text
hotspot   stdout ____ lines   stderr ____ lines
cratonvm  stdout ____ lines   stderr ____ lines
PROBE-MANIFEST-DIGEST   hotspot ________________  cratonvm ________________
PROBE-LEDGER            hotspot ________________  cratonvm ________________
divergent observables   ____        (this record's figure: 9)
stderr sidecar lines    ____ hotspot / ____ cratonvm
```

**Do not copy the 9, the 864 or `22732607802c59c2` into that block.** They are
this record's measurement on the release binary of 2026-08-12 00:21, and a
remembered figure written where a fresh one belongs is how a stale oracle gets
its second life. If a fresh digest differs from `22732607802c59c2`, the probe
gained or lost declarations since — which is expected and is not by itself a
defect; it means the row counts are not comparable to the ones above.

Run `-VmArgs "--jdk-only"` as a second arm. This record's 9 is `--real-jdk`
only, and nothing has ever taken this probe through the `--jdk-only` arm the
campaign is named for.

## What was NOT changed

- What `Enum.valueOf` throws, or its message. Another lane owns it, and it
  remains one of the nine.
- `ArrayDeque`, `CopyOnWriteArrayList` or anything else in
  `native-collections`. The five rows that looked like defects are not.
- Any assertion in the probe. Every change makes it louder: nothing was
  relaxed, no bound was widened, and the 858 observables and their values are
  identical.
