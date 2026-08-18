# G40-1 — the index reconciled: 280 files, 36 with no row, and seven claims that were wrong

**Status:** META. **Documentation only — this lane edited no `.rs` file, no file
under `regression-suite/`, and ran no `cargo` command.** Every number below is
either a file-system count taken by this lane, or a measurement taken by another
lane and cited to it by name. **This record measures nothing about the VM.**

**Provenance.** Branch `claude/jdk-only-mode-completion-1351c0`, `HEAD =
9ae371468`, 2026-08-17. Listing taken with
`ls docs/known-issues/jdk-only/*.md | wc -l`. Git used read-only throughout
(`git log --oneline`, `git show --stat`, `git status --short`); no
state-changing git command was run, including `git stash`. The binary was not
run; the one thing this lane executed against the filesystem outside the docs
directory was `ls -la "$JAVA_HOME/lib/src.zip"`, to check a claim rather than
repeat it.

**Files changed:** `INDEX.md`, `README.md`, sixteen records that received a
banner, and this file. Nothing else.

---

## 1. The count

**280 `.md` files** when the listing was taken. There are no non-`.md` entries in
the directory. **This record makes it 281**, and five more (`G35-1` … `G39-1`)
were being written by live lanes during the pass and are not counted here — so
the honest form of the number is *"280 at `9ae371468`, already stale."*

| pass | date | files | rows missing when taken |
|---|---|---|---|
| C18, first | 2026-08-13 | 155 | — (the snapshot itself) |
| F25, second | 2026-08-13 | 227 | 73 |
| third (measurement) | 2026-08-14 | ~234 | 7 added |
| **G40, fourth** | **2026-08-17** | **280** | **36** |

The 36 with no row were the entire wave-G line (`G1-1` … `G34-1`, 34 records)
plus `BASELINE-20260817.md` and `HANDOFF-20260814.md` — which are, respectively,
the most current document in the directory and the one every lane brief points
new readers at. Both are now rowed, in `INDEX.md` §A.

**One false alarm, resolved by hand.** A first-cell scan reports the eight
SSL-chain records (`E3-1`, `E12-1`, `E22-1`, `E31-1`, `E42-1`, `F6-1`, `F10-1`,
`F18-1`) as unrowed. They are rowed; that table's first column is a sequence
number. The method used, so the next lane can repeat rather than trust it:
extract every table row's first cell from `INDEX.md`, strip backticks and bold,
and test each filename's first 18 characters against that set. It has one
false-positive mode (numbered tables) and one false-negative mode (a record
named only in prose, never in a row).

## 2. What was changed

1. **`INDEX.md`** — a **FOURTH PASS** block appended, in the house convention
   that each pass lands as a dated block rather than being merged into C18's
   topic tables, so each snapshot stays legible as the snapshot it is. It has
   §A (the 36 added rows), §B (the seven reconciled claims), §C (the statuses
   measurement settled), §D (what this lane could not settle). The header
   carries a fourth warning blockquote pointing at it.
2. **`README.md`** — the headline record count corrected. See §4.
3. **Sixteen banners.** Listed in §3.
4. **This record.**

Column meanings are unchanged: `Status` is read from **each record's own status
prose**, never from its filename or title; `Prov` is where the numbers came from
(`MEAS` / `PRED` / `SRC` / `MIXED`). Partly-fixed records stay `OPEN` with
`partial` in Notes, because a half-closed record is a trap if it reads as closed.

## 3. The seven claims, and the sixteen banners

Full statements with evidence classes are in `INDEX.md` §B. In brief:

| # | claim now falsified | falsified by | evidence |
|---|---|---|---|
| B.1 | `invocations == 0` proves a body dead | `G33-1`; `G31-1` for the `asType` instance | MEASURED, causally isolated |
| B.2 | `force_native_over_real_jdk_bytecode` is the gate | `G34-1` (`9ae371468`) | MEASURED, both directions, cold and warm |
| B.3 | the JDK's sources cannot be read on this host | `BASELINE-20260817`; first used by `G22-1` | FILESYSTEM, re-verified here |
| B.4 | `moving_young: cycles=0` means a broken young path | `G27-1` (`3765fad76`) | MEASURED, 96 runs |
| B.5 | the `gc::guard` W7-84 warning is a reference-slot census | `G25-1`, widened by `G30-1` | MEASURED + source trace |
| B.6 | a red vector is "one assertion from green" | lane G21, via `--only=<family>` | MEASURED on an existing binary |
| B.7 | a suite pass count says something about `--jdk-only` | `BASELINE-20260817` | MEASURED, three separate runs |

**Banners placed** (blockquote immediately under the title, in the style the five
`aastore` records already carry; **no record's history was rewritten**):

| record | why |
|---|---|
| `HANDOFF-20260814` | §4's registry recommendation, §3's `jdk25src` path, §1's "two remain red", §6.1's "nothing clever is needed" |
| `G20-1` | GC headline falsified; §8's `invocations` claim not reproducible; measured on `target-fcheck` |
| `G15-1` | "the registry dump is taken at registration time" |
| `G16-1` | §8 concludes `plain_socket.rs` is off the path from a zero |
| `G17-1` | §1.3/§4's "loses nothing measured"; also records its vector going GREEN |
| `G18-1` | `MethodHandle.asType` "not the live body either" — falsified outright |
| `G24-1` | §7.3's "`invocations=0` confirms it" |
| `G28-1` | restates `G17-1`'s inference; also records its vector going GREEN |
| `G1-1`, `G4-1`, `G7-1`, `G8-1`, `G9-1`, `G11-1` | "no JDK source was read" — `src.zip` was there |
| `F5-1`, `F14-1` | the force-list reachability argument is void |

Two of those banners record a **confirmation** as well as a correction (`G17-1`,
`G28-1`): their vectors went green, MEASURED. A banner in this directory is not
only a mark of error.

## 4. `README.md` — corrected, not deferred

The old headline read *"OPEN, **94 records**"*, with a paragraph admitting that
both terms of the arithmetic `105 − 11` had moved and cancelled. It is now the
**file count, 280 on 2026-08-17 at `9ae371468`**, stated as a file count, with
the README's own exclusion rule applied mechanically (10 pattern-matched, 17
dated deliverables matching no pattern) to give **253 as an explicit upper bound
on defect records, not a count of them** — because roughly a dozen files in the
remainder are `META` by their own prose and match no naming pattern. The old
paragraph's best sentence is kept: **the exclusion list is the thing that
drifts, not the count.**

This is the fix the previous headline asked for and did not get: a number that
says what it is measuring.

## 5. What this lane could NOT settle

Repeated here because the directory's standard is that an honest `unknown` beats
a confident label, and because `INDEX.md`'s own header documents a case where a
wrong status "is a trap if it reads as closed".

1. **Whether `F5-1`'s and `F14-1`'s `CharBuffer` conclusions survive `G34-1`.**
   Their *argument* is void. Their *answer* needs a registry dump and a
   behavioural probe on the current binary, from a lane that may edit Rust. Their
   banners say the argument is void and stop there — they do not claim the
   conclusion is wrong.
2. **How many "dead body" conclusions in this directory rest on a zero.** Seven
   sites were found by targeted grep. The phrasing varies too much for a grep to
   be a census, so treat that list as a **floor** — the same shape as the
   instrument it is about.
3. **Which of `G20-1`'s non-GC numbers are affected by its binary.** It measured
   `target-fcheck`, which `G34-1` says to ignore. `G27-1` re-measured the GC
   claims on a good binary; startup, throughput and the ~141 ns native boundary
   were not re-taken. They are marked **unre-measured**, not wrong.
4. **`G35-1` … `G39-1`.** Being written as the count was taken; no rows. Rot,
   observed in the act.
5. **`README.md`'s ~86 per-record rows.** Not audited row by row. Only the
   headline was corrected.
6. **Whether any pre-wave-G record's status prose is stale for a reason this
   session did not touch.** This pass reconciled the seven claims it was given
   plus what the suite measured. It did not re-read 244 older records and makes
   no claim that their statuses are current. `HANDOFF-20260814` §2 remains the
   right default: **every record older than `F41-1` states PREDICTED outcomes,
   and a prediction is not a result.**

## 6. The one thing to carry forward

The two instruments this directory trusts most were both wrong this session, in
the same direction: **they under-report, and a lane read the under-report as an
absence.** `invocations` counts registry-resolved dispatches and was read as a
call count; the W7-84 warning counts one line of `vm_object.rs` and was read as a
census of native reference-slot writes. In both cases the correction is the same
shape — *this number is a floor* — and in both cases the earlier reading led a
lane to conclude that something was not happening.

`owns_slot` plus a behavioural probe settles liveness. A zero settles nothing.
