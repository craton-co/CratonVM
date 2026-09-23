# Wiring `check-orphan-instruments.sh` into CI

**Added:** 2026-09-21, round 10, lane `diagread`.
**Script:** [`scripts/check-orphan-instruments.sh`](../../scripts/check-orphan-instruments.sh)
**Baseline:** [`scripts/baselines/orphan-instruments-allowlist.txt`](../../scripts/baselines/orphan-instruments-allowlist.txt)

This page exists because the lane that wrote the gate does not own
`.github/workflows/ci.yml` and must not edit it. Everything needed to wire the
gate is here; the decision is the orchestrator's.

## What the gate is for

Round 10 found the same defect five times, in five unrelated files, by five
lanes that were not looking for each other's work:

| Instance | Shape |
|---|---|
| `jit::exec_memory::unregister_range` | withdrawal entry point with no caller — every implicit-null entry outlived its buffer |
| `metrics::osr_compile_declined` | metric row nothing ever incremented; read `0` on every run that has ever existed |
| `metrics::record_osr_event` | silently no-ops on a name absent from `OSR_EVENTS`, so a typo'd feeder reads identically to no feeder |
| `code_cache_lifecycle::record_allocation_failure` / `record_capacity_bytes` / `record_free_space` | three gauges with no feeder |
| `implicit_null::stale_duplicates_retired` / `unlinked_chain_nodes` / `base_mismatches` | three "should read zero forever" counters with no reader outside `#[cfg(test)]` |

One shape, and one sentence explains why it is dangerous every time: **zero is
indistinguishable from "this never happened".** Every other class of bug in this
tree produces a wrong answer, and a wrong answer has a test. An instrument with
no wiring produces a correct-looking answer forever, and the operator reading it
concludes the hazard it watches did not occur.

Five in one round is where hunting instance six by hand stops paying. The gate
is the mechanical version of the hunt: it does not fix anything and it does not
claim the tree is clean, it claims the population does not **grow**.

## What it checks

* **C1** — `pub fn record_*` / `pub fn note_*` whose body touches an atomic,
  with no call outside its defining file.
* **C2** — `pub fn NAME()` (zero arguments, no `&self`) returning an integer, a
  tuple of integers, an `Option<int>` or a `Vec<(..)>`, whose body touches an
  atomic, with no call outside its defining file.
* **C3** — a row of a `*_EVENTS: [&str; N]` table that is named nowhere else:
  its literal occurs only in the table AND no `TABLE[i]` reference to its index
  exists anywhere.

  The index arm is not decoration. A literal-only rule reported four of this
  tree's 37 rows as unfed and all four were wrong: `SCHEDULING_EVENTS` is fed
  entirely by index (`record_scheduling_event(SCHEDULING_EVENTS[6])`, 37 such
  references over 8 rows) and `DEOPT_STASH_EVENTS` is a *pull* table whose
  getter names both rows by index. With the arm, C3 reports zero unfed rows on
  this tree — and would still have caught `osr_compile_declined`, because
  `OSR_EVENTS` has **zero** `OSR_EVENTS[` references anywhere and every one of
  its feeders spells the literal.

The script's header states the limits in full (dynamic dispatch, names built at
runtime, FFI, `cfg`-compiled-out calls, cross-file `#[cfg(test)]` callers, and
C3's inability to distinguish a feeder from a reporter). The short version: **it
proves "no textual call site", never "no caller"**, and it proves nothing about
whether a live caller ever fires.

## The `ci.yml` step — **wired 2026-09-22**

This was a suggestion until the register reached two entries. It is now the step
that is actually in `.github/workflows/ci.yml`, immediately after `Forbid
unscreened Value-cell decodes`, verbatim apart from a longer comment saying why
it could finally be turned on. See "Fourth execution" at the end of this page.

It belongs in the same block as the other two bash ratchets — after the
build/test steps, beside `check-no-diag-prints.sh` and
`check-value-cell-reads.sh`, gated to the Linux leg for the same reason (it is a
bash-only script) and for one more: it shells out to `rg` several hundred times
and is measurably cheaper on the Linux runner.

```yaml
      # An instrument nobody reads cannot warn anyone. Round 10 found the same
      # defect five times in five files: a counter, gauge or metric row with no
      # production reader or no feeder, so it reads zero forever -- and zero is
      # indistinguishable from "the hazard it watches never happened". Ratchets
      # against a frozen allowlist; fails only on a NEW orphan.
      # Bash-only script, so gate to the Linux leg.
      - name: Forbid new orphaned instruments
        if: matrix.os == 'ubuntu-latest'
        shell: bash
        run: bash scripts/check-orphan-instruments.sh
```

Insert it immediately after the `Forbid unscreened Value-cell decodes` step
(`ci.yml` around line 291 at `33957575f`). Nothing else in the workflow changes;
the gate needs no build artefacts, no toolchain beyond `bash`, `awk`, `sed` and
`ripgrep`, and no `target/` directory.

## Before you turn it on, read this

**1. The gate has never been executed.** The lane that wrote it was not
permitted to run repository tooling. What *was* run is the gate's own `rg` and
`awk` searches, as separate invocations, against the worktree at `33957575f`
plus that lane's two source edits — that is where the 112 frozen entries come
from. The assembled script was **read, not executed**, apart from `bash -n`
(a parse-only syntax check, which passed).

So the first CI run is the gate's first real run. If it is red, the
overwhelmingly likely cause is a transcription difference between those
invocations and the assembled script, **not** 112 new defects. The remedy is one
command:

```sh
scripts/check-orphan-instruments.sh --update-allowlist
```

which rewrites the baseline from the census that run took and prints the delta.
Land that delta as a correction, in a commit that says what moved and why.

**Recommended:** run it once locally, or land the step as a non-blocking leg for
one cycle, before making it blocking. A gate that is red on arrival gets
disabled rather than fixed — that is the failure mode this whole page is written
around.

**2. Runtime.** The two heavy passes are a multi-pattern fixed-string `rg` over
the workspace (measured at about **8 seconds** on this tree during the census
runs) and a per-row `rg` loop for C3 (37 rows at `33957575f`, up to two `rg`
calls each). Budget under two minutes. The fixed-string form is load-bearing: the same
names as a regex alternation with `\b` anchors took **over two minutes**, because
`rg` can only drop to Aho-Corasick when every pattern is a literal.

**3. `rg` is required and there is no fallback arm.** Unlike
`check-no-diag-prints.sh`, this gate exits 3 rather than falling back to
`find`+`grep`. A second implementation of the matching rules would be a second
thing to keep in step with the frozen allowlist, and a baseline taken with one
matcher and checked with another is how a ratchet starts reporting fiction. The
GitHub Ubuntu runner has `ripgrep` available via `apt`; if the current image does
not carry it, add `sudo apt-get install -y ripgrep` to the step, do **not**
weaken the matcher.

**4. Exit codes.**

| Code | Meaning |
|---|---|
| 0 | no orphan outside the allowlist |
| 1 | at least one new orphan — the failure the gate exists for |
| 2 | no allowlist file; a ratchet cannot report without a baseline |
| 3 | the gate itself is broken (missing `rg`, a positive control failed, no `*_EVENTS` rows found at all) |

Exit 3 is not a lint failure, it is the gate refusing to report a clean tree it
never read — the same discipline as `check-no-diag-prints.sh`'s sentinel and
`untyped-alloc-ratchet.sh`'s zero-guard. Treat a 3 as a build break on the gate,
not on the change under test.

**5. The two positive controls are pinned to a real function.**
`implicit_null::stale_duplicates_retired` must appear in the candidate census
(proving the definition pass ran) **and** must be seen as called from a file
other than `jit/src/implicit_null.rs` (proving the call-site pass and the
cross-file classification ran). That second caller is the
`[cratonvm] implicit null-check table health:` line in `vm-cli/src/main.rs`,
added in the same change as this gate. Deleting either is a legitimate edit —
retire the sentinel in the same commit and say what replaced it.

## What a green run does not prove

* It does not prove any instrument is *correct*, only that something calls or
  reads it somewhere in the source text.
* It does not prove a caller is ever **reached**. `unregister_range` with a
  caller behind a condition that never holds reads exactly like a healthy one.
  That needs a run.
* It does not prove the 112 allowlisted entries are acceptable. They are a debt
  register. Several are certainly the same defect as the five above — the three
  code-cache gauges in the list are literally the subject of an open
  known-issues page.

## First execution, by the orchestrator (2026-09-21)

The lane that wrote this gate was not permitted to execute repository tooling,
so it shipped the script and the frozen allowlist as *read, not executed*, and
asked for exactly this: one real run before anyone makes the gate blocking. That
run happened, and it is the reason the two paragraphs below exist rather than a
green tick.

**The first run was red, and the gate was wrong, not the tree.** It aborted with
`awk: fatal: expression for '<' redirection has null string value`, then fired
its own positive control (§5 above): `stale_duplicates_retired` "is not seen as
called from any file other than `jit/src/implicit_null.rs`". It *is* called —
`vm-cli/src/main.rs:1050`. The cause was the call-site pass being invoked as
`awk '…' NAMES="$TMP/names" "$TMP/hits"`. A command-line assignment is applied
when awk REACHES it in the operand list, which is after `BEGIN` has run, so
`NAMES` was the empty string exactly where the census is loaded. The census
therefore held nothing, *every* name came back uncalled, and the control fired
on a healthy tree. Fixed to `awk -v NAMES=…`, which is set before `BEGIN`.

That failure is worth keeping in writing, because it is the shape the gate
itself exists to catch, one level up: an instrument whose reader is wired but
whose plumbing silently reads nothing, and whose output — "no caller anywhere" —
is indistinguishable from the real defect. The positive control is what turned a
silently-empty scan into a loud refusal, which is precisely the argument §5
makes for having it.

**With that fixed the gate exits 0 on this tree**, and reported two allowlisted
names that had since gained a reader (`note_young_spill_pressure`,
`record_access`). Re-frozen with `--update-allowlist`: 112 → 110 entries, two
removed and none added, which is the direction this ratchet is for.

### Provenance notes the allowlist header used to carry

`--update-allowlist` rewrites that header, so anything durable belongs here.

* **Version 1's 112 lines** were produced by running this gate's own `rg`/`awk`
  searches as separate invocations against the worktree at `33957575f` plus lane
  `diagread`'s two source edits — the searches were run even though the
  assembled script was not. The first real execution above disagreed by exactly
  two names, both in the good direction, which is the evidence that the
  hand-assembled census and the script agree.
* **No `row` entry is frozen**, and that is a result rather than an omission:
  all 37 `*_EVENTS` rows in the tree at `33957575f` are fed. An earlier,
  literal-only version of check C3 called four of them unfed; all four were fed
  by INDEX (`record_scheduling_event(SCHEDULING_EVENTS[6])`), and adding the
  index arm removed all four rather than allowlisting them.
* **Three entries are an open bug, not accepted debt**:
  `record_allocation_failure`, `record_capacity_bytes` and `record_free_space`
  are the three gauges of
  `docs/known-issues/jit/r10-gcs2-code-cache-alloc-failure-gauges-never-fed-20260921.md`.
  Lane `gauges` closed that page in this same round by making the VM PULL the
  numbers from the JIT rather than having the JIT push them, and deliberately
  kept the push wrappers (deleting them trips a different frozen ratchet,
  `vm/tests/no_test_only_public_api.rs`). So they stay listed here as what they
  now are: wrappers with no caller, whose job was taken over by a different
  direction of data flow.

## Second execution (wave 6): one real catch, one false positive

The gate was run again against the round's wave-6 tree, and both halves of the run
are worth recording, because two lanes had been asked to PREDICT which allowlist
entries their changes would retire. A prediction that matches is evidence the gate
measures what the lane reasoned about; one that does not is a finding. This run
produced one of each.

**The real catch, and the first one this gate has made.** It failed with two names
that were not on the allowlist at all: `block_offset_no_jump_census` and
`g1_evac_copy_span`, both in `gc/src/g1.rs`. Round 10 changed nothing under `gc/`,
so they arrived through a `dev` merge from a concurrent session — a `pub fn`
returning a counter, written by production, read by nothing. Exactly the shape the
gate exists for, caught mechanically rather than by a lane reading a file. They are
frozen with the reason recorded in
`docs/internal/fixed-bugs/r10-gate-two-g1-censuses-have-no-reader-FIXED-20260922.md`, because
`gc/src/g1.rs` is not this round's file and another session is working in it.

**The false positive.** The two lanes predicted 8 removals between them; 7 happened.
The one that did not was `note_long_long_value_direct_site`, and it did not because
**the gate cannot see a same-file production caller**. That notifier is defined at
`jit/src/lib.rs:11347` and called at `jit/src/lib.rs:31703` — a real production call
site on the single-pass door, added by lane `readers` precisely to fix the orphan.
The gate discounts it under the rule stated above: "a reference in the DEFINING file
is never a caller."

That rule is right for a small module, where the only same-file references are the
definition, a wrapper and a `#[cfg(test)]` block. It is wrong for `jit/src/lib.rs`,
which is over 30 000 lines: in a file that large, "same file" stops being a proxy
for "not a real caller" and the exclusion starts hiding genuine wiring. The entry
therefore stays on the allowlist as a gate limitation rather than as debt, and this
is the note that says so — a future reader must not conclude from its presence that
the counter is unread.

The fix is not to drop the same-file rule, which is what keeps a definition and its
own wrapper from looking like a call. It is to distinguish a reference inside the
defining item or a test block from one in an unrelated function in the same file,
which needs scope awareness the current text scan does not have. `docs/feature-designs/jit-r10-wrappers-proposals.md`
already argues for teaching this gate that `#[cfg(test)]` is not `pub`; this is the
same class of blindness from the other side, and both want the same remedy.

## Third execution (2026-09-22): the two G1 names closed, and a checked escape hatch

Both halves of the wave-6 run above are now resolved, and they wanted different
answers because they were different defects wearing the same output.

### `block_offset_no_jump_census` was a real orphan; it and its two siblings now
### have a production reader

The gate was right. Nothing outside `gc/tests/` read it, so on a shipped binary
`CRATONVM_G1_BLOCK_OFFSET` could be turned on and produce no reading at all. The
sweep that followed found the same of both its siblings, which is why the fix
carries three names rather than one: `block_offset_process_census`,
`block_offset_no_jump_census` and `block_offset_audit_census` answer one question
between them and none of them means anything read alone —
`jumps=0` has three causes, `tail_breaks`/`no_entry` are what separate them, and
`audit_violations` is a documented must-be-zero that says the producer rule the
whole optimisation rests on has broken.

They are read by a new `[GC] g1 block-offset:` line in
`gc_metrics::collector_decision_report`, which is the report `vm-cli` emits on
BOTH shutdown arms, and pinned by `gc/tests/g1_w3c_report_reachability.rs` — on
`REQUIRED_LINES`, so the line cannot silently leave the report, and on
`the_census_lines_print_their_zeros`, so no later edit can wrap it in an
`if jumps > 0` and hide the census in exactly the state it is wanted in.

### `g1_evac_copy_span` was never an orphan, and the allowlist can now say so

`g1::g1_evac_worker_census_report` reads it and formats the
`[GC] g1 evac-copy-span:` line; that report reaches a shipped binary through the
same `collector_decision_report`. The chain leaves the crate — the first hop just
does not leave the FILE. This is the L8 false positive, and it is the same shape
as `note_long_long_value_direct_site` in the wave-6 run above.

The span could not simply be moved to `gc_metrics.rs` beside the other
`[GC] g1 ...` lines, which would have made the question moot: four of that line's
fields are computed from the per-worker rows, so the two halves have to be
assembled together.

So the gate gained a narrow, opt-in, **checked** exemption instead — a
`// orphan-gate: read-by <FN>` line in the comment block attached to the
definition. It is not a mute button; it is a claim the gate re-verifies on every
run, on both halves:

* `<FN>` must be defined in the same file and its body must really contain a call
  to the instrument (scanned to the first column-0 `}`); and
* `<FN>` must itself be called from another file.

If the reader is deleted, renamed, or loses its last cross-file caller, the
exemption lapses and the instrument is reported again. An orphan therefore cannot
exempt another orphan, which is how a dead report would otherwise launder a dead
counter. The annotation sits on the instrument rather than on the reader, because
that is where the next person reading the definition needs it.

Two design points are recorded in the script's header and worth repeating here:

* **The annotation must be ATTACHED to the definition.** The first version of the
  pass searched the whole file, and every uncalled instrument in `gc/src/g1.rs` —
  a 30,000-line file with eight of them — inherited the one annotation written
  for `g1_evac_copy_span`. An exemption that leaks to its neighbours is worse than
  no exemption.
* **"Any same-file call counts" was measured and rejected.** On this tree, on
  2026-09-22, that rule would have rescued more than sixty of the 103 allowlisted
  orphans, almost all of them through a same-file `#[cfg(test)]` test — i.e. it
  would have absolved the exact five finds the gate was built from.

### What this does NOT fix: `note_long_long_value_direct_site`

Its chain is two hops, and the middle one is private:
`note_long_long_value_direct_site` ← `build_single_pass_tables` (a file-private
`fn` in `jit/src/lib.rs`) ← the compile entry points. The annotation's second half
asks whether the named reader is called from another file, and a private function
never is, so annotating it would be rejected rather than honoured — correctly,
under the rule as written. Closing that one needs either file-local reachability
(every `fn` in the file that is transitively called from one with a cross-file
caller) or the scope awareness the wave-6 note above asks for. It stays on the
allowlist, still as a gate limitation rather than as debt.

### Allowlist delta

```
REMOVED (an instrument that gained a reader -- good):
  - fn block_offset_audit_census
  - fn block_offset_no_jump_census
  - fn block_offset_process_census
  - fn g1_evac_copy_span
ADDED (a new orphan -- say why in the commit):
```

103 entries → 99. Nothing added.

## Fourth execution (2026-09-22): 98 → 2, and the step goes on

Every remaining allowlist entry was dispositioned by hand, one at a time, and
the baseline was re-frozen at **2**. The gate exits 0 against it:

```text
candidates (C1+C2): 495
orphaned fns:       2
read-by exempt:     0
event rows scanned: 38
unfed rows:         0
allowlisted:        2
```

### What each of the 96 got

| Disposition | Count | Where it is recorded |
|---|---|---|
| **Wired** | 54 | `vm/src/runtime/instrument_census.rs` and `jit/src/instrument_census.rs` — an opt-in per-subsystem report (`CRATONVM_INSTRUMENT_CENSUS=1`) that prints every row including the zeros |
| **Narrowed** — `pub fn` → `pub(crate) fn` | 33 | at each definition |
| **Deleted** — function, static and tests | 7 | a comment in its place naming the question it answered and where that question is answered now |
| **Feeder restored** | 1 | `interpreter::execute`'s exec-depth ceiling |
| **Closed by a concurrent lane** | 1 | `note_long_long_value_direct_site` |

**Wired** is this page's own prescription ("a counter getter → read it in
vm-cli's method-stats dump") with one deliberate difference: the report is on
its OWN switch, not folded into `jit.method_stats`. It spans eight crates, and a
reader who asked for JIT method statistics did not ask for the GC's
block-offset census.

**Narrowed** is the resolution the wave-6 note above asked for in prose when
`note_long_long_value_direct_site` turned out to have a real same-file caller
30 000 lines away. A third of the register was that shape. `pub(crate)` states
it in the type system, and — unlike the `read-by` annotation — needs no check,
because the compiler is the check.

**Feeder restored** is the one live defect in the batch.
`dispatch_trace::note_bb_dispatcher_cap_hit` documented itself as used by the
S-bytebuddy r2 hard cap in `interpreter::execute`; that cap had been rewritten
as the H6 per-thread exec-depth ceiling and the call was lost in the rewrite.
The ceiling is what converts an uncatchable native stack overflow into a
catchable `StackOverflowError`, so its rate is a real question. The call is
back.

### The two that stay, and why they are not annotated

`g1::note_mark_cycle_outcome` and `zgc::metrics::record_allocation_stall` are
both **fed** in their own files (by `cleanup` and by `ZgcStallGuard::drop`) and
are `pub` only because `gc/tests/` integration tests drive them. That is the
feeder-side twin of L8: the gate's rule cannot see a same-file feeder, and it
does not search `**/tests/**` at all.

They deliberately carry **no** `orphan-gate: read-by` annotation, although one
would make them pass. Half two of that check is a textual search for `<FN>(`
outside the defining file, and the only candidate names here are `cleanup` and
`drop` — which match unrelated functions in `difftest`, in `vm`, and in
essentially every file in the workspace. The annotation would be verified, and
verified against somebody else's code, which is the failure mode L8's design
notes are careful about. So they are allowlisted, and the reason is written at
each definition rather than in the baseline file.

That is the difference between an allowlist that is a backlog and one that is a
decision.

### The step is on

`.github/workflows/ci.yml` now runs the gate on the Linux leg, beside
`check-no-diag-prints.sh` and `check-value-cell-reads.sh`. The "before you turn
it on" section above was written when the baseline was 112 entries and a gate
that arrives red-ish gets ignored rather than fixed. At two entries it is a
ratchet somebody will read, and the next `+ fn <name>` in the baseline is a
decision made on purpose.

## Fourth execution (2026-09-22): a shape this gate is the wrong instrument for

Round 10 wave 8's lane `offsetkey` found `jit/src/osr_exit.rs::exceptional_reason_at_bci`
— a `pub fn` whose only references outside its own declaration were two asserts
in its file's `#[cfg(test)]` module — and **this gate could not have seen it**.
Not by a near miss: C1 matches `record_*`/`note_*`, C2 a zero-argument
integer-returning `pub fn` that touches an atomic, C3 an `*_EVENTS` row. That
item is a two-argument enum-returning **verdict function** touching no atomic.

The framing this file opens with is "an instrument nobody reads cannot warn
anyone". The class is wider than instruments: a refusal, a classifier or an
admission test with no production caller reads as a guard that is in place and
holding, and has never run. §1 of
`docs/feature-designs/jit-r10-offsetkey-proposals.md` proposes a check C4 for
exactly that, and it is still a proposal, because it needs the SAME scope-aware
same-file exclusion the wave-6 false positive above wants — verdict functions
cluster in large modules, so C4 would hit that blind spot harder than C1/C2 do.

What did land is the other half, and it is recorded here so a reader does not go
looking for C4 to explain it: `vm/tests/no_test_only_public_api.rs` now scans
`jit/src` declarations as well as `vm/src`, under a second frozen baseline. That
ratchet's offender predicate already described the item exactly; only its
declaration sweep was `vm`-only, while its reference sweep already read every
workspace member. The instance itself is closed by giving the function a
production caller. See
`docs/internal/retired/r10-offsetkey-exceptional-reason-at-bci-is-unwired-and-cannot-refuse-20260921-RETIRED-20260922.md`.
