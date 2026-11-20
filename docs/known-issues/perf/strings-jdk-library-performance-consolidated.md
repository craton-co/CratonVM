# JIT performance: JDK strings, regex, startup variation and small calls

**Status: OPEN.** The five earlier issue pages have been archived as historical
records and consolidated here. Their archival does not mean their performance
gaps or acceptance gates have passed. This issue covers authentic OpenJDK
execution in both default and `--compatible` modes.

**2026-11-09:** eleven opt-in switches are now default ON; 13 of 14 R24 rows are at or below 2.8x HotSpot in both modes and `compile` (3.72x) is the one row still over 3.0x. The tables below are the earlier opt-in diagnostics; the current numbers are in the last section.

**2026-11-09 (late):** late LICM over the predicated regex loops (last section) cut `matchonly` 17%, `cached` 13% (now 1.84x) and `split-ws` 11%. Six rows are below 2.0x; `matchonly` (2.30x), `split-ws` (2.23x), `regex-split` (2.18x), `regex-find-email` (2.57x), `regex-groups` (2.68x), `regex-find-ci` and `regex-matches` (2.71x) are not, and `compile` stays at 3.73x.

**2026-11-10:** fused branches and guards, allocation-based field proofs, a shorter copy tail and a short-trip vector exit (last section): `matchonly` 2.30x -> **1.90x**, `cached` 1.64x; seven rows are below 2.0x. Still above: `regex-split` (2.02x), `split-ws` (2.06x), `regex-find-email` (2.28x), `regex-groups` (2.32x), `regex-matches` (2.49x), `regex-find-ci` (2.66x), and `compile` (3.71x).

**2026-11-11:** dev armed ZGC's load barrier by default (concurrent relocation, `1bb72dfd67`), and with it the regex rows counted 1.5x-15x more instructions (`regex-matches` 8,817 -> 13,476, `regex-find-email` 21,149 -> 312,833). Two armed causes were fixed (lambda-proxy captures, reference `System.arraycopy`); the rest is spread over many helper paths, so concurrent relocation is opt-in again (`CRATONVM_ZGC_CONCURRENT_RELOCATE=1`; it also cost 25-40% mutator throughput on a fragmenting live set without bringing pauses near HotSpot's). The default is back to the unarmed counts this page uses. Closed and retired: the public feature contracts.

**2026-11-10 (late):** memory-phi frame words leave the fresh-frame clear, and a short broadcast fill stores its tail under a mask (last section): `regex-split` **1.99x**, `regex-groups` 2.20x, `matchonly` 1.85x; eight rows are below 2.0x. Still above: `split-ws` (2.01x), `regex-groups`, `regex-find-email` (2.38x), `regex-matches` (2.42x), `regex-find-ci` (2.65x), and `compile` (3.67x).

**2026-11-10 (evening):** field proofs now reach through spliced loops, cover per-trip receivers, sit on pre-header hoists and on loaded references (last section): `split-ws` **1.98x**, `regex-find-ci` 2.65x -> 2.29x, `regex-find-email` 2.13x, `regex-groups` 2.10x, `regex-split` 1.86x; nine rows are below 2.0x. Still above: `regex-matches` (2.35x), `regex-find-ci`, `regex-find-email`, `regex-groups`, and `compile` (3.66x).

**2026-11-11 (`Matcher.reset` attempt):** late LICM past trapping pre-headers, empty/1–3-element scalar bypass on primitive vector fills, and `ExactClassIs`/compact proofs for `String.coder` & proven-receiver loads (last section): `mat-reset` 1,662 -> **1,494** (-10.1%, HotSpot 600, 2.49x; `--compatible`: 1,494), `mat-reuse` 7,058 -> **6,792** (-3.8%; `--compatible`: 6,795), `regex-matches` 8,813 -> **8,647** (-1.9%; `--compatible`: 8,675), `regex-find-ci` 16,788 -> 16,511 (-1.6%). The >= 2x gap-reduction target on `mat-reset` (`<= 1,131`) remains open.

## Current evidence and remaining gaps

The latest integrated diagnostic executable, built from feature source
`0766d3747c`, has SHA-256
`b2c714ae074a4bdab6b492c9357af134e79a4e00bb684b5a8037ce2f6ed326a0`.
One independently checked instruction repetition per row and mode, using an
experimental register-allocation/chain/profile policy, gives:

| R24 row | Default / HotSpot | Compatible / HotSpot | Status |
|---|---:|---:|---|
| `compile` | 3,591 / 803 (4.470x) | 3,563 / 803 (4.435x) | Above 3.0x |
| `regex-matches` | 12,214 / 3,798 (3.215x) | 12,236 / 3,798 (3.221x) | Above 3.0x |
| `regex-find-ci` | 21,516 / 7,330 (2.935x) | 21,516 / 7,330 (2.935x) | Below in this screen |
| `matchonly` | 20,636 / 7,019 (2.940x) | 20,636 / 7,019 (2.940x) | Near threshold |
| Other ten rows | Below 3.0x | Below 3.0x | One diagnostic repetition |

This is not the final production-policy gate. The two failing rows require
further work, and all near-threshold rows require repeated final measurement.
The same candidate passed 12 primitive-store and 12 checked-folded-array cases
across both modes, three collectors and poisoned/unpoisoned high bits, the 14
regex facts in each mode, and 20 strings correctness comparisons. The raw
windows, source identity and checks are under
`/root/regex-split-evidence-20261107/register-aware-path/integration-dev-5e681`.

The small-JDK-method/charset issue remains open. The current diagnostic
measurements appear below; its historical Windows probe reported
`CharsetDecoder.decode` 190–250 ns versus HotSpot 9–10 ns and `ByteBuffer.wrap` 58–70 ns versus
5 ns. The original suite workloads included `TestMethodPerformance` and
`TestHttp2Section_8_2`, which exceeded a 600 s cap on a 4-vCPU host. Those
Windows and suite observations are historical; the original suite workloads
still need current measurements.

## History and findings

- **October 2026 — regex and split.** The original regex rows showed per-call
  costs of several multiples of HotSpot despite correct Java-visible results.
  The work identified compiled-call chains, inlining admission, large
  conservative frames, guard duplication, allocation and register/home
  traffic. Comma splitting and other fast rows improved; authentic JDK bytecode
  and exception semantics remain authoritative.
- **October 2026 — strings.** Internal String admission, case-insensitive
  comparison and code-point prefixes were implemented. The separate
  `StringBuilder.length` switch remained off after its A/B failed the
  published flip rule. Scope includes G1–G5 correctness, engagement and
  timing, not just the prefix implementations.
- **October 2026 — small calls and charset.** A stuck-interpreted tiering
  defect was fixed, but `CharsetDecoder`, buffer setters and
  `ByteBuffer.wrap` retained large gaps. The historical sampler found nested
  buffer/bridge calls and frame clearing; plain allocations became much
  closer to HotSpot once the loop body was spliced. Earlier attribution of
  TLAB retirement at blocking calls was a hypothesis, not a confirmed cause.
- **November 2026 — startup levels.** Captures identified a near/far layout
  epoch encoding difference, and a separate displaced-C1-counter promotion
  defect was fixed. Those observations did not explain all later compatible
  mode levels. Ten-process stability must be established on the final binary.
- **November 2026 — register-aware path.** Selection/allocation, checked
  memory operands and GC/deoptimization workstreams landed on dev. Subsequent
  changes shortened a low-address reference-read barrier check, admitted
  resolved `Object[]` allocations to splices, and omitted the generational
  owed-cycle poll for G1/ZGC. They improved the diagnostics above without
  closing the two remaining rows.

The complete original histories and their detailed measurements remain
available as archived records:

1. [Strings proposals and original gate details](../../feature-designs/README.md).
2. [Regex per-call history](../../feature-designs/README.md).
3. [Compiled split and regex history](../../feature-designs/README.md).
4. [Startup/compile-placement history](../../feature-designs/README.md).
5. [Charset and small-call history](../../feature-designs/README.md).

The [retirement work log](../../feature-designs/README.md),
[register-aware plan and current evidence](../../feature-designs/README.md),
and [strings G1–G5 scope](../../feature-designs/README.md)
contain further commands, experiments, fallbacks and historical dispositions.

## Work remaining

1. Profile and reduce the remaining `Pattern.compile` and
   `regex-matches` instruction costs on the integrated production path.
   Preserve the validated split and `Node.match` chain optimizations and
   verify actual compiled-body identity and engagement.
2. Extend the current charset, buffer and small-call measurements to the
   final candidate and reproduce the original suite workloads against HotSpot 25.
   Attribute the present costs with actual compiled bodies, call paths, allocation/GC and timing data; fix remaining
   material regressions and verify the original suite workloads, including the
   historical 600 s timeout condition.
3. Finish the strings G1–G5 checks and final integration: retain the historical
   seven-process interleaved timing/control protocol, prefix engagement,
   G3 keep-off/flip evidence and G4 replacement-workload provenance. Recheck
   capture-group allocation and inline integer rendering on actual paths.
4. Resolve or explain the separate startup-placement and compilation-shift
   observations. Check ten processes per required arm and mode on the final
   executable; retain process values, compile populations and raw evidence.
5. Run the entire 14-row HotSpot 25 instruction matrix in both modes under
   one shipping policy. Every row must be at or below 3.0x, with repeated
   clean windows, correct thread attribution, utilization and compile-clock
   checks. A diagnostic or experimental policy alone does not satisfy this.
6. Verify Java output, exceptions, regex/split, strings, deoptimization and
   moving-GC behavior against HotSpot in both modes and the required
   collectors. Review and integrate all landed work, commit, merge the
   verified branch into dev, push and verify remote commits, then remove only
   task-created target directories.

Keep this issue open until every applicable requirement above has final-source
evidence. Record binary hashes, source revisions, exact commands, environment
and independent readers; do not infer completion from an archive move.

## 2026-11-08 charset and residency diagnostics

On candidate `b2c714ae074a4bdab6b492c9357af134e79a4e00bb684b5a8037ce2f6ed326a0`,
seven interleaved processes per arm ran `DecLoop` with one million iterations,
five in-process repetitions and the median of the final three repetitions
per process. Process medians in ns/op (HotSpot / default / compatible):
`dec` 11.8 / 145.9 / 143.6, `wrap` 5.6 / 49.0 / 49.2, `pos` 1.1 / 72.6 /
72.4, and `sb` 5.0 / 64.9 / 73.9. The Craton arms used the experimental
register/chain/profile policy, not a final production decision. This is a
current baseline, not a correctness or suite-workload closure. Raw process
outputs, commands and binary identity are in
`/root/regex-split-evidence-20261107/register-aware-path/integration-dev-5e681/charset-baseline`.

Two independently checked interleaved instruction repetitions also retested
the existing opt-in reference-residency policy after the memory-operand
handoff. On default mode, `compile` rose from 3,591 to 9,132 instructions
and `regex-matches` from 12,214 to 25,812 with residency enabled; restricting
it to safepoint-free ranges still gave 8,918 and 22,761. Compatible showed
the same qualitative regression. The policy remains disabled; the cause of
this measured regression has not yet been established. Raw measurements and
verification are in the sibling `ref-residency-study` directory.

## 2026-11-08 outlined poll register map

With reference residency enabled, `Pattern.compile` fell back to the baseline
backend because install-time deoptimization verification found an integer in a
register marked as a GC reference. An outlined poll emitted after the body was
using the end-of-body register owners rather than the owners at its branch.
A regression test first reproduced the wrong register bit; outlined polls now
capture their own owners and use them when publishing the deferred map.

The 19 poll-map, 48 deoptimization-metadata and 13 allocated-result tests
passed, as did the VM release check and CLI build. Candidate SHA-256:
`59dd9fee60d9a02386a5e7d5920f40ff57cf5409e73eafad0ba9b77cabce82a8`.
The previously rejected parser compiled in the optimizing tier. Twelve
forced-collection store cases, four both-mode regex fact runs with normal or
forced deoptimization, and 90 reference-read comparisons including concurrent
ZGC and color flips matched HotSpot. The collector stress used reference
residency on.

Two interleaved diagnostic repetitions show the catastrophic fallback is gone,
but residency is not a win on the two failing rows: default `compile` is
3,591 instructions with it off and 3,624 on; `regex-matches` is 12,214 off
and 12,365 on. Compatible has the same direction. Restricting residency to
safepoint-free ranges also does not close either row. It remains opt-in.
Evidence, red/green tests, trace and raw PMU windows are under
`/root/regex-split-evidence-20261107/register-aware-path/integration-dev-5e681/outlined-poll-owner`.

A separate one-repetition diagnostic screen on the poll-map candidate tried
linear-scan split transitions, lazy reference homes, lazy frame-block clearing
and disabling deopt-register naming. None improved both failing rows: split
transitions were approximately neutral, while lazy frame clearing and
turning off deopt-register naming increased their counts. These are screening
results, not flip decisions or correctness evidence. See
`/root/regex-split-evidence-20261107/register-aware-path/integration-dev-5e681/allocator-policy-study`.
The current charset diagnostic confirms optimizing bodies for `DecLoop.main`
and `ByteBuffer.wrap`, and shows a protected caller-frame allocation boundary
refusing one `wrap` splice. That boundary protects exception/deoptimization
semantics and cannot be removed without proving them; its presence alone does
not establish that it accounts for the observed timing gap.

The poll-map candidate `59dd9fee60d9a02386a5e7d5920f40ff57cf5409e73eafad0ba9b77cabce82a8`
completed a ten-process diagnostic startup screen in both modes using the
experimental policy. All raw windows passed the independent reader. Default
medians for HotSpot/CratonVM were `regex-groups` 5,696/16,412, `compile`
803/3,590 and `regex-matches` 3,798/12,232 instructions/op. Compatible
medians were 5,803/16,412, 803/3,564 and 3,798/12,218 respectively.
CratonVM process spread was at most 0.15% across those arms; HotSpot groups
spread was up to 2.46%. This is not final shipping-policy validation, and the
compile and matches ratios remain above 3.0x. Raw runs and reader are under
`/root/regex-split-evidence-20261107/register-aware-path/integration-dev-5e681/outlined-poll-owner/startup`.

Two interleaved diagnostic repetitions on the poll-map candidate compared
the actual default JIT policy with the experimental register/chain/profile
policy, keeping one C1/C2 worker in each Craton arm. Default-mode instruction
medians were: `compile` 3,763 default-policy versus 3,590 experimental and
HotSpot 804; `regex-matches` 13,407 versus 12,223 and HotSpot 3,796;
`regex-find-ci` 21,639 versus 21,516 and HotSpot 7,330. Compatible medians
were 3,735 versus 3,564, 13,411 versus 12,218, and 21,639 versus 21,516,
with the corresponding HotSpot values 803, 3,796 and 7,329. The default
policy remains over 3.0x on compile and matches too. This screen neither
flips a flag nor supplies the final 14-row production gate. Raw windows,
commands and independent verification are in
`/root/regex-split-evidence-20261107/register-aware-path/integration-dev-5e681/shipping-policy-study`.

A one-repetition policy decomposition on the same candidate separates the
experimental gains. In default mode `regex-matches` was 13,407 instructions
with the default policy, 12,990 with register allocation, and 12,201 with
register allocation plus the two chain overrides. Branch profiling with
register allocation alone gave 13,006; the combined experimental policy gave
12,232. For `compile`, the corresponding values were 3,760, 3,703, 3,703,
3,590 and 3,591. Compatible mode had the same ordering. One HotSpot
`regex-find-ci` control landed at 9,863 rather than the usual approximately
7,330 instructions, so this screen is diagnostic and no ratio is inferred
from that cell. Each counted window passed the independent reader, but one
repetition and an unstable control are insufficient for a default flip.
Raw artifacts are in the sibling `chain-policy-study` directory.

A full 14-row one-repetition screen of the actual default JIT policy on
candidate `59dd9fee60d9a02386a5e7d5920f40ff57cf5409e73eafad0ba9b77cabce82a8`
used one C1 and one C2 worker and no experimental JIT overrides. The
independent reader found no rejected windows. Four rows exceeded 3.0x in both
modes: `compile` 4.688/4.651x, `regex-matches` 3.530/3.531x,
`regex-groups` 3.193/3.060x and `matchonly` 3.186/3.186x (default /
compatible). `split-ws` was 2.899/2.900x and `regex-find-ci` 2.952/2.952x,
close enough to require repeated measurements. The other eight rows were
below 3.0x. A single repetition is a diagnostic baseline, not acceptance.
See `/root/regex-split-evidence-20261107/register-aware-path/integration-dev-5e681/shipping-full-screen`.

A one-repetition chain-tree size screen at 325, 650, 1,000 and 1,300 bytes
on the same candidate and experimental policy was effectively neutral for
`compile`, `regex-matches` and `regex-find-ci` in both modes. The default-mode
HotSpot `regex-find-ci` control again landed at the higher 9,863-instruction
level, so no ratio is inferred from that cell. This does not justify a policy
change. Raw windows and independent checks are in the sibling
`chain-size-study` directory.

## 2026-11-08 legacy-field cold helper

The current matcher trace repeatedly took a jump after a successful rowless
legacy field load. For unprotected, non-colored, non-referent sites that
already meet the existing cold-tail admission, the legacy read now falls
through and the same checked helper is outlined. Slot count, compact-layout,
reference tag, null and receiver checks retain their failure edges.

The native field suite passed nine tests plus the compact-tail regression; a
further native fixture covers integer, long and reference fields, including
short objects, null, compact-layout and wrong-tag helper fallbacks. Twelve
forced-collection store cases and both-mode regex facts with normal and forced
deoptimization matched HotSpot. Ninety reference-read comparisons, including
concurrent ZGC and color flips, also matched. The tested production code has
SHA-256 `5bfce696d60619e7b22cf28300ca2d263d414f3f43e1ad5e5f05f57f5bcf6da3`;
the subsequent source changes only expanded the native test and clarified a
comment.

Two interleaved diagnostic repetitions give default `regex-matches` 12,223
before and 12,184 after, and compatible 12,218 before and 12,170 after;
HotSpot was approximately 3,796 instructions. `compile` was unchanged.
The row remains above the 3.0x gate. Exact source patch, test history,
commands and raw measurements are in
`/root/regex-split-evidence-20261107/register-aware-path/integration-dev-5e681/legacy-field-tail`.

## 2026-11-08 proven-field cold helper

Proven compact-field reads now use the existing deferred checked-helper tail
when its ordinary safety admission holds. The successful read falls through;
a cleared read-barrier table, changed layout epoch or other failed guard still
uses the checked helper. Colored barriers and sites the existing admission
rejects retain their inline paths. A native fixture exercises a live and a
cleared reference-read table with the tail both enabled and disabled.

The release CLI built from the recorded source patch has SHA-256
`494fc9f4d75f6de2c848fb55b64663d70e9c717b6dbeebbaf5d6198849fc6274`.
Ten focused field tests, the compact-tail regression, and the release VM check
passed. Twelve forced-collection primitive-store cases across both modes and
three collectors, four both-mode regex fact runs with normal or forced
deoptimization, and 90 reference-read comparisons including concurrent ZGC
and color flips matched HotSpot.

Two independently checked interleaved instruction repetitions compared that
binary to the previous field-tail candidate under the experimental policy.
Default-mode medians, old/new instructions per invocation, were `compile`
3,590/3,553, `regex-matches` 12,166/11,918, and `regex-find-ci`
21,505/20,819. Compatible medians were 3,566/3,528, 12,179/11,931 and
21,516/20,819 respectively. These are diagnostic measurements, not a final
shipping-policy gate: `compile` and `regex-matches` remain above 3.0x HotSpot.
The first screen was discarded because its copied launchers selected the old
binaries; the corrected screen additionally checks the launch paths against
the binaries whose hashes it records. Source patch, commands, correctness
outputs and both screens are under
`/root/regex-split-evidence-20261107/register-aware-path/integration-dev-5e681/proven-field-tail`.

A subsequent two-repetition screen used the actual shipping policy and checked
its executable paths and PMU windows. On the proven-field candidate, default /
compatible instruction medians were `compile` 3,717 / 3,691,
`regex-matches` 13,102 / 13,106, `regex-groups` 17,765 / 17,435,
`matchonly` 21,962 / 21,963, `regex-find-ci` 20,941 / 20,941 and
`split-ws` 36,095 / 36,139. Corresponding HotSpot medians were 803,
3,796 / 3,795, 5,803 / 5,836, 7,019, 7,330 and 12,596. The first four
rows remain over 3.0x in at least one mode. This is a six-row diagnostic,
not the final 14-row gate. Raw data and independent reader are in the
`proven-field-tail/shipping` directory beside the evidence above.

## 2026-11-08 allocation-proven field receivers

Inline field reads now reuse the SSA allocation and phi non-null proof already
used for field stores. A successful allocation produces a non-null reference;
a failed allocation leaves the compiled path before its result is used. Other
receivers retain their null checks. The native fixture checks both cases and
the nullable fallback. The release binary has SHA-256
`1bc3452ffc61453ec0a738145b54c5fcd5ef0ddc02b702aa13a73e4d82f83534`.
Eleven focused field tests, the compact-tail regression, release VM check,
12 collector-stress cases, four normal/forced-deoptimization regex fact runs,
and 90 reference comparisons including concurrent and color-flipping ZGC
passed.

In two interleaved experimental-policy repetitions, `regex-matches` fell from
11,918 to 11,694 instructions in default mode and 11,931 to 11,698 in
compatible mode, against HotSpot 3,798. `compile` fell from 3,553 to 3,541
and 3,527 to 3,514 respectively, against HotSpot 803–804. The two rows
remain above 3.0x and require further work; these diagnostic policy results
are not a shipping gate. The exact patch, binary hashes, commands, results and
independent reader are in the sibling `field-allocation-null` directory.

## 2026-11-08 register-aware workstream audit

The 01 selection/allocation and 03 GC/deoptimization retirement claims were
premature. In the current tree, the allocator and reference residency are
opt-in, MIR still turns off operand-in-place emission, and the selected
allocator consumes only a subset of instruction families. The 01 page itself
requires the complete path to be the normal production entry. A unit test of
selected move and condition-code acceptance is not evidence that every
comparison, branch, call, edge copy and exceptional route consumes one
allocated machine plan.

The 03 report also described IR return anchoring as enabled, while
`ir_return_anchor_enabled` uses `runtime_flag_on`, which is false when unset;
its inventory row has been corrected to opt-in. `register_frame_value` names
integer and long registers but falls back to homes for references. Thus the
claimed end-to-end contract for register-only references is not established.
The selected reference-residency path has useful moving-GC and deoptimization
coverage, but the outlined-poll owner bug fixed in commit `73ce6f61f2` shows
why those checks do not by themselves prove every deferred helper, poll,
exception, monitor and inspection edge. Continue the edge-by-edge map and
relocation audit, test actual machine-location transitions and fallback paths,
and enable a production policy only after its full correctness and performance
gates pass. These findings identify missing completion evidence and one fixed
fallback defect; they do not establish a new live GC corruption bug.

The opt-in cold-edge-set proof was retested on the allocation-null candidate
under the experimental register/chain/profile policy. Two interleaved
repetitions in each mode reduced `compile` from 3,541 to 3,443 instructions
(default) and 3,514 to 3,415 (compatible), versus HotSpot 803; the selected
matching and case-insensitive rows did not materially improve. It remains an
opt-in diagnostic and does not close the gate. The A/B uses the same binary
on both sides; paths, hashes, PMU windows and reader are in
`/root/regex-split-evidence-20261107/register-aware-path/integration-dev-5e681/field-allocation-null/edge-set`.

With allocator, reference residency and IR return anchoring explicitly on,
the current candidate passed 90 reference comparisons against HotSpot across
both modes and G1, Generational, ZGC, concurrent ZGC and color-flipping ZGC.
Four diagnostic processes showed reference residency admitted and invalidated
cached copies. This supports those tested cases, not complete coverage of all
moving-GC and deoptimization edges. Raw outputs, commands and the candidate
hash are in the sibling `audit-reference` directory.

A further `J27DeoptRows` audit used the same candidate with allocation,
reference residency and IR return anchoring explicitly on. All 12 combinations
of both modes, G1/ZGC/Generational, and normal/eager chain-and-guard exits
matched HotSpot. Each case compiled the all-types, OSR, spliced, exception and
unstable-branch methods; the residency census reported admitted and invalidated
references, and the forced back-edge collection census reported at least 3,988
collections per case. An independent reader rechecked raw output, hash,
compilation engagement, residency counts and collection counts. The test does
not establish that every named reference lived in a register at a moving
collection, or cover every deferred edge. Raw evidence is in the sibling
`audit-deopt` directory.

The cold-edge-set proof was also screened against the unmodified shipping
policy on the same candidate, two interleaved repetitions in both modes.
`compile` fell from 3,707 to 3,620 instructions in default mode and 3,682
to 3,594 in compatible mode (HotSpot 803). The other five sampled rows were
approximately neutral, with small variation in `split-ws`. The four failing
rows still exceed 3.0x; this six-row screen cannot establish a default flip or
closure. Raw windows, verified binary paths and reader are under the sibling
`edge-shipping` directory.

## 2026-11-08 refreshed shipping-policy matrix

**Isolation correction:** another session's Cargo build began at 11:16:07 UTC,
while the compatible-mode run (11:13:45–11:18:19 UTC) was in progress.
Its compatible results are contaminated and do not satisfy the no-build rule,
even though the PMU windows passed the reader. The default arm ended before
the build started. Rerun compatible mode on an idle host.

A fresh full 14-row screen used the allocation-null candidate
`1bc3452ffc61453ec0a738145b54c5fcd5ef0ddc02b702aa13a73e4d82f83534`, the
shipping policy and one independently checked repetition per arm and mode.
All 56 process cells and their 168 counted windows were accepted. Default / compatible ratios above 3.0x
were `compile` 4.615 / 4.575, `regex-matches` 3.400 / 3.391,
`regex-groups` 3.058 / 3.058 and `matchonly` 3.128 / 3.128. The other ten
rows were below 3.0x in this diagnostic. The screen is not the repeated
final gate. It refused its first launch while a build was active, and the
successful run began after a fresh host process check. Raw windows, commands,
binary identity and the independent reader are under
`/root/regex-split-evidence-20261107/register-aware-path/integration-dev-5e681/shipping-refresh-1bc`.

The compatible half was rerun after the observed build ended, using the same
candidate and policy. The independent reader accepted all 28 process cells
and 84 counted windows. `compile` was 4.584x, `regex-matches` 3.396x,
`matchonly` 3.128x, and `regex-groups` 2.998x HotSpot; the other ten rows
were below 3.0x. This is one repetition, and groups is too close to call
without repeated final measurements. Raw data and the reader are under
`/root/regex-split-evidence-20261107/register-aware-path/integration-dev-5e681/shipping-compatible-isolated-1bc`.

## 2026-11-08 shipping-policy startup screen

On candidate `1bc3452ffc61453ec0a738145b54c5fcd5ef0ddc02b702aa13a73e4d82f83534`,
ten default-mode processes per arm ran `regex-groups`, `compile` and
`regex-matches` with one C1/C2 worker and the shipping policy. An independent
reader accepted all counted windows. Craton instruction medians/ranges were
17,748 / 17,700–17,748 for groups (0.27% spread), 3,708 / 3,706–3,708 for
compile (0.04%), and 12,878 / 12,878–12,886 for matches (0.06%). HotSpot groups
varied 2.43%; its compile and matches spreads were 0.00% and 0.29%. The raw
process values, binary hash and reader are in `shipping-startup-1bc` under
`/root/regex-split-evidence-20261107/register-aware-path/integration-dev-5e681`.

A process monitor observed another Cargo build start after the default arm had
completed but during compatible mode; that partial compatible arm was stopped
and discarded. A separate compatible rerun was also stopped and discarded when
a new build started. No compatible startup conclusion is drawn from either.
This is a current diagnostic, not the final binary's stability gate.

The compatible startup screen was rerun after the observed build ended, with
a monitor that recorded no overlapping Cargo or Rust compiler process. Ten
processes per arm produced Craton medians/ranges of 17,417 / 17,417–17,417
for groups, 3,682 / 3,682–3,682 for compile, and 12,882 / 12,882–12,883 for
matches. Their maximum spread was 0.01%. HotSpot's respective spreads were
2.43%, 0.13% and 0.13%. The independent reader checked all windows and
binary identity. Raw values and monitoring are in the sibling
`shipping-startup-compatible-retry-1bc` directory. These measurements do not
replace stability and correctness checks on the final integrated candidate.

## 2026-11-08 matcher trace follow-up

A single `regex-matches` invocation traced with the experimental policy on
candidate `1bc3452ffc61453ec0a738145b54c5fcd5ef0ddc02b702aa13a73e4d82f83534`
executed 11,732 instructions: 6,761 in the row caller, 4,792 in
`Pattern$BmpCharPropertyGreedy.match`, and 179 outside the JIT map. The
trace has 4,021 moves, 1,137 equal branches, 1,031 not-equal branches and
867 tests. These are one invocation's instruction counts, not timing or the
shipping-policy gate.

The optimizer census for the greedy body reports four field-class loops
examined, zero planted and zero reads replaced; four candidate refusals say a
class test is not a guarded splice's. The saved graph includes two successive
exact-class tests of the same field read before the miss call, i.e. a
bimorphic shape. This is a concrete next investigation for the performance
owner: determine whether a guarded invariant-field proof or loop versioning
can handle both alternatives while preserving the miss call, writes, GC
barriers, OSR and precise deoptimization. The existing single-class proof
cannot simply assume either class. The trace and graph are in the sibling
`field-allocation-null/trace-matches` directory.

The class-test-fusion A/B was attempted under the shipping and experimental
policies but every attempted screen either refused to start with a build
active or was stopped after the build monitor observed an overlap. No
performance conclusion or policy flip is claimed from those partial runs.

In that one matcher trace, a reader joined executed instruction addresses to
the saved disassembly: the greedy body executed 33 comparisons against the
first observed lambda class and nine against the second, plus 21 and nine
comparisons at the two observed node-class sites. The class IDs identify that
capture only. These counts quantify engagement of the bimorphic paths; they
do not prove an optimization can remove them or safely move their associated
field reads. The reader is `field-allocation-null/trace-matches/class-tests.py`.

Against the corresponding HotSpot counts in the latest diagnostic matrices,
the default-mode remaining reductions to the 3.0x line are approximately
1,297 instructions/op for `compile` (35.0% of its current count), 1,515 for
`regex-matches` (11.8%), 338 for `regex-groups` (1.9%), and 895 for
`matchonly` (4.1%). Compatible needs approximately 1,272, 1,502 and 895 for
compile, matches and matchonly respectively; its single `regex-groups` sample
was only ten instructions below the line. These are estimates from one
process per cell and are not substitute acceptance thresholds or proof of
stability. They show that compile needs a substantially larger improvement
than the small class-test changes measured so far.

## 2026-11-08 class-test fusion screen

Using candidate `1bc3452ffc61453ec0a738145b54c5fcd5ef0ddc02b702aa13a73e4d82f83534`,
a clean single-repetition experimental-policy A/B enabled the existing
`IR_FUSED_CLASS_TEST_BRANCH` flag: default / compatible `regex-matches`
fell from 11,694 / 11,698 to 11,344 / 11,348 instructions against HotSpot
3,798. This puts those individual diagnostic samples just under 3.0x. The
case-insensitive row fell by about 539 instructions; compile was effectively
unchanged. One repetition under an experimental policy is not a flip or gate.
Raw data and reader: sibling `field-allocation-null/class-fusion-single`.

A separate A/B used the shipping policy plus the two existing class-fusion
switches, with two interleaved repetitions. The default-mode half completed
before the driver was terminated during compatible mode; its completed half
was independently checked and the build monitor was empty. Median default
instructions were: matches 12,878 -> 12,561 (HotSpot 3,798), groups
17,747 -> 17,596 (HotSpot 5,784), matchonly 21,953 -> 21,813 (HotSpot
7,019), and compile 3,707 -> 3,704 (HotSpot 803). Matches, groups and
matchonly remain above 3.0x in this screen. The unfinished compatible half is
not a result. Raw data and the default-only reader are in the sibling
`field-allocation-null/class-fusion-shipping-clean` directory. Repeat on the
final integrated source, validate semantics and all rows, and use the project's
flip criteria before changing a default.

The compatible half was rerun to completion with two repetitions and no
observed build overlap. Its independently checked medians were matches
12,882 -> 12,565 (HotSpot 3,798), groups 17,418 -> 17,267 (HotSpot
5,784), matchonly 21,953 -> 21,813 (HotSpot 7,019), and compile
3,676 -> 3,674 (HotSpot 803). Groups is under 3.0x in this small screen;
compile, matches and matchonly remain above. The case-insensitive and
whitespace-split controls also improved. Raw data, build monitor and reader:
sibling `field-allocation-null/class-fusion-compatible`. This remains an
opt-in A/B, not a correctness validation, default flip or final gate.

Class-test fusion correctness follow-up: `R24RegexSplitRows facts` matched
HotSpot in both modes with G1, ZGC and Generational collectors, with normal
and eager chain/guard exits (12 cases), on the allocation-null candidate with
the two class-fusion switches enabled. The separate 200,000-warmup
`Rx27ScanLoopSubjects` run timed out after 300 seconds in both default-mode G1
arms with forced collections; the remaining subject cases were stopped.
Those timeouts are not parity passes and require a suitably bounded or longer
run, including proof of compiled-path engagement. Raw output and commands are
under `field-allocation-null/class-fusion-correctness`. This does not approve
a default flip.

## 2026-11-08 integrated candidate: chain and residency diagnostics

Combined source `722b71b27d`, binary SHA-256
`0cd031f767f323430ade871c5bedf920447678540b6cf37dccaaf5a58a618b3e`,
was built after the latest selection and GC/deoptimization work landed. One
verified shipping-default repetition per row and mode left default compile,
regex-matches and regex-groups above 3.0x; compatible compile and
regex-matches were above. See the performance workstream record for the
counts and raw windows; one repetition is not a final gate.

An opt-in chain-bimorphism attribution screen found default regex-matches
12,081.89 -> 11,277.79 instructions/op and regex-groups 17,486.48 ->
16,422.11, while compile was unchanged near 3,594. Its subsequent full
matrix reproduced the improvement in both modes but overlapped a brief Git
push on the host. Treat that full screen as diagnostic and repeat without
overlap before using it to assess a default change. The switch is still
opt-in; broad regression and correctness gates remain. Raw evidence and
independent readers are in `integration-722b/policy-attribution` and
`integration-722b/bimorphic-full` under
`/root/regex-split-evidence-20261107/register-aware-path/`.

One independently checked reference-residency repetition per arm on this
integrated candidate showed no earlier baseline-backend-sized regression.
However full residency cost default compile 3,593.78 -> 3,641.76, matches
12,085.30 -> 12,247.79, and groups 17,486.20 -> 17,872.24 instructions/op.
Restricting it to local safepoint-free ranges was also slower. Compatible
showed the same direction. These samples do not justify enabling the option.
Raw windows and checker are in the sibling `integration-722b/ref-residency`
directory.

The same candidate with chain bimorphism enabled matched HotSpot's warmed
`R24RegexSplitRows facts` output in all 12 combinations of default and
compatible modes, G1/ZGC/Generational collectors, and normal/eager
chain-and-guard exits with forced back-edge collections. This is focused
correctness evidence, not proof of all deoptimization and collector paths.
A separate one-operation debug trace recorded 11,332 compiled instructions
with the option versus 12,136 without it. The baseline trace entered the
separate `LastNode.match` body 714 instructions; the option's trace had no
separately executed `LastNode.match` region. This identifies an engaged
chain-splice change, not a PMU count or proof of a final policy decision.
Outputs, environment, binary hash and trace captures are under
`integration-722b/bimorphic-correctness` and `integration-722b/trace-pair`.

A clean two-repetition allocator on/off A/B on the combined candidate gives
default medians (off -> shipping on): compile 3,664.57 -> 3,595.79,
regex-matches 12,557.82 -> 12,081.81, regex-groups 17,707.51 ->
17,486.48 instructions/op. Compatible has comparable savings. This confirms
that the default allocator produces a measurable improvement, while its
observed savings are smaller than the remaining gap. Raw evidence and the
independent reader are under `integration-722b/allocator-effect-idle`.

A repeated interleaved full 14-row A/B of the opt-in chain pair, with two
processes per cell and matching thread/counter/compile-clock verification,
found default matches 12,081.8 -> 11,277.8 (2.971x HotSpot) and groups
17,486.9 -> 16,421.1 (2.854x); compatible matches 12,084.7 -> 11,280.7
(2.970x) and groups 17,156.0 -> 16,421.0 (2.848x). The largest sampled
increases were under 1%; compile stayed at 4.477x / 4.446x. The run
checked that no build was present at five-minute snapshots; those snapshots
do not prove absence of shorter overlaps. This experiment leaves the option
off pending its broader flip criteria and does not close the gate. Raw data
and verifier are under `integration-722b/bimorphic-repeat`.

One-repetition parser attribution checked three existing opt-ins. On default,
branch profiling alone gave 3,463.66 instructions per compile, the cold-edge
set proof 3,508.78 and the lean start-bit store 3,581.91, against baseline
3,597.48 and HotSpot 803.23. Together they gave 3,347.67; compatible showed
the same direction. A separate default-only budget screen gave 3,466.84
with a 2,600-byte inline budget against 3,598.84 baseline; 5,200 bytes did
not improve it further. These are diagnostic opt-ins, not validated default
changes or a fix for the remaining 3.0x gap. Raw records and readers are in
`integration-722b/parser-options` and `integration-722b/parser-budget`.

Single-invocation traces of the current default candidate and HotSpot show
3,302 versus 753 instructions in their respective parser chains. Craton's
`Pattern.compile`, `sequence`, and `escape` regions accounted for 1,225,
401, and 365 instructions, respectively; HotSpot's corresponding regions
accounted for 412, 157, and 74. These traces are diagnostic, not equivalent
to the whole-row PMU count. The Craton debug census reports 17 dropped home
stores out of 114 for `Pattern.compile`, 9 of 92 for `sequence`, and 8 of
110 for `escape`; this supports further investigation of emitted frame and
register traffic without attributing every remaining instruction to it. Raw
traces, maps and disassembly are in `integration-722b/trace-parser`.

Ten-process startup diagnostic with the opt-in chain pair and the same
integrated binary produced default/compatible median instructions of
16,421.68 / 16,421.41 for groups (each spread below 0.001%) and
11,277.79 / 11,291.28 for matches (each spread 0.16%). HotSpot's groups
medians were 5,764.25 / 5,725.80 and its matches median was 3,798.48
in both arms. The independent reader checked per-thread PMU utilization and
unchanged compilation clocks in each counted window. Five-minute build
snapshots cannot exclude shorter overlap, and the run did not capture a
complete compilation-population census; it is not the final startup gate.
Raw evidence and reader are under `integration-722b/bimorphic-startup`.

A simple disassembly/trace classifier counts 627 frame stores and 385 frame
loads in the sampled Craton parser chain, versus 15 stack stores and 53
stack/frame loads in the corresponding HotSpot trace. This classification
includes prologue clearing and is not a claim that all stores are removable.
The classifier and disassembly are in `integration-722b/trace-parser`.

A one-repetition default-mode diagnostic also enabled each existing
`IR_ALU_REG_OPERAND`, `IR_LS_SPLITS`, `IR_PARAM_HOME_LIVENESS`, and
`IR_CALLEE_OWNED_ARGS_SPLICE_REPLAY` option separately. Neither compile
(3,593.78 baseline; 3,593.78–3,598.45 with an option) nor matches
(approximately 12,082 in every arm) improved materially. The attempted
options do not establish how to remove the sampled frame traffic. Raw
windows and the independent reader are under `integration-722b/frame-options`.

The sampled Craton frame-access trace breaks down into 385 value loads,
320 value stores, 64 XMM stores, 228 recognizable zero stores, and 15
nonzero immediate stores. This is an instruction-shape classification, not
a proof about value liveness or removable GC publication. The script is
`integration-722b/trace-parser/frame-breakdown.py`. In a separate
one-repetition default diagnostic, disabling fresh-frame bulk clear raised
compile from 3,593.45 to 4,203.78 instructions, disabling its AVX form
raised it to 3,738.76, and disabling primitive-colour claims raised it to
3,743.74. Those existing switches are not shortcuts to the gate; raw
windows and reader are in `integration-722b/frame-clears`.

The opt-in chain candidate matched the `J27DeoptRows` HotSpot output in
all 12 mode/collector/eager combinations with forced back-edge collections;
each case reported compiled probe methods and collections. This focused
test does not establish every GC/deopt edge. Evidence is under
`integration-722b/bimorphic-deopt`.

The integrated candidate's full release JIT library test run is not green.
It reported multiple assertion failures and crashed in a lowering test that
passed a fabricated PIC address to an emitter now reading the slot. The
fixture was changed to use live PIC slots and its formerly crashing test
passed; a serial rerun advanced past it but still reported 31 failures before
a second crash in `r23_w6_ir_ref_arraycopy_prefix_copies_inline_or_calls`.
The isolated second crash executed generated code reading a fabricated
reference `0x1001`; its underlying cause has not been resolved. One
`ic_guards_use_published_slot_offsets` assertion also still fails after the
live-slot change. The suite is not a pass and no policy flip is justified
by these results. Logs, GDB captures, and the isolated fixture test result
are in `integration-722b/bimorphic-jit-tests`.

A proposed larger change, including phased ownership, safety contracts,
measurement points and the limits of the approximately 2x stretch target,
is scoped in [position-aware lowering and parser call-cost reduction](../../feature-designs/README.md).
The proposal is only partially started and the issue remains open.

The array-copy test crash described above was isolated to a fixture that
passed integer values as references into an element-type check. Replacing
the mismatch case with live, nonmatching test objects makes that isolated
test pass. A subsequent serial suite run progressed further but still
crashed in `the_ir_register_first_inline_arm_sets_the_bit_then_runs_post_init`
after jumping to the fixture's `0x1000` slow-path placeholder; this third
crash and the suite assertion failures remain unresolved.

## Position-aware lowering implementation started (2026-11-08)

The first phase adds an opt-in diagnostic position map when both
`CRATONVM_DBG_JITC` and `CRATONVM_DBG_IR_GRAPH` are enabled. Each data-node
record includes the executable-buffer address, scheduled block, node and
bytecode index, selected rule, emitted byte range, input/result frame homes,
assigned GP registers, publication state and dropped-home status. The address
joins records to the actual captured compiled body rather than guessing which
recompilation produced it. The map is diagnostic: it does not yet cover
terminators, deferred slow paths, full instruction constraints, or prove the
location contract. Capture and address-joined reader are under
`/root/regex-split-evidence-20261107/register-aware-path/location-census-20261108/trace2`.

A first experimental conversion route emits signed `I2L` directly from an
already-published GP register into its assigned result register, preserving
the existing home and carry/refusal rules. It is disabled by default under
`CRATONVM_JIT_IR_ALLOCATED_I2L`; the selected instruction uses the shared x64
encoding table. Native unit coverage exercises same/different low and high
registers, signed boundaries, poisoned high bits, unavailable sources and
the disabled fallback. The existing allocated-selection tests and flag
surface/document tests pass. A prior unguarded two-process A/B showed no
material improvement on compile, matches or groups; the route is therefore
not a shipping-policy change or evidence that the 3x gate is met. Evidence:
`location-census-20261108/i2l-ab` and `i2l-refusal-test.log`.

The third serial-suite crash in the IR allocation test came from an
unaligned synthetic cursor and a placeholder slow-path address after alignment
testing became default. The fixture now explicitly selects its intended
align-up path; its nine focused allocation tests pass. The full serial suite
continues to fail other assertions and later crashes in a separate
`runtime_lowering::r21_alloc_post_init_helper_first_tests` case. Neither the
full correctness gate nor the architectural proposal is complete.

A second experimental route, `CRATONVM_JIT_IR_ALLOCATED_LCMP=1`, compares
two already-published GP operands directly before the existing signed
three-way result materialization. It refuses carried and scratch-register
operands. Native tests cover operand order, signed extremes and fallback.
In two interleaved repetitions per arm and mode on one binary, compile
changed from 3,595.64 to 3,593.29 instructions in default and 3,570.75 to
3,568.53 in compatible; matches was unchanged. Enabling this route with
experimental `I2L` gave 3,591.79 and 3,566.42 respectively. These small
results do not justify enabling either flag by default. Raw counters,
verifier and binary hashes are in `location-census-20261108/lcmp-ab`.


The in-repository reader `tools/probes/jit_ir_location_trace.py` joins the
location census to captured instruction addresses and rejects overlapping or
out-of-body node ranges. In the sampled `Pattern.compile()V` body it mapped
971 of 1,225 instructions to data nodes; 254 remained outside those ranges.
The largest mapped categories include array allocation, reference stores and
loads, integer loads, and calls. These are counts of executed instructions in
selected node ranges, not a proof of removable cost. Buffer-address reuse is
not fully disambiguated by this diagnostic; additional attempt identity and
coverage of terminators and slow paths are required for a complete census.

The scoped architectural proposal remains the next avenue, not a demonstrated
2x result: it combines a position-specific selection/allocation/emission and
GC/deoptimization location contract with profile-led call and allocation
reduction. Neither the full correctness suite nor the original performance
retirement gate passes, so this issue stays actionable and the experimental
routes stay disabled by default.


A further serial JIT-suite attempt fixed the same unaligned synthetic-cursor
assumption in `r21_alloc_post_init_helper_first_tests`; both tests in that
module pass. The suite progressed to a separate crash in the single-pass x64
`the_helper_arm_registers_before_it_commits` test and reported 86 panics on
the way. A debugger observed an invalid pointer passed to `free` while the
fixture dropped its compiler; the cause is not established. This remains a
blocking correctness investigation, not a passing suite or evidence of a
production defect. Logs and debugger output are in
`location-census-20261108/post-init-fixture`.

The next investigation isolated the x64 crash to test harnesses that populated
the cached frame slot but not the thread-local mirror used by generated code.
Publishing the fake thread in the mirror for the helper and register-first
fixtures makes the two previously crashing focused tests pass. The full
serial release JIT suite now completes: **7,634 passed, 85 failed, 8 ignored**.
It remains a failed correctness gate; the complete output is in
`location-census-20261108/x64-register-mirror/full.log`.


The location census now delimits lowering attempts. Its reader rejects a
captured address with multiple accepted attempts or any later address reuse,
and requires an explicit `--allow-legacy` for logs without attempt markers.
The marker identifies an accepted lowering, not an installation or code
lifetime; without time-correlated load/unload data any reuse after acceptance
is deliberately refused. A fresh parser trace from the rebuilt launcher mapped
971 of 1,225 `Pattern.compile()V` instructions to data nodes and left 254
unmapped. This confirms the earlier sample for that body while leaving
terminators, prologues and deferred tails outside the census. Six reader
regression tests passed. Build, trace and attribution are under
`location-census-20261108/attempt-attribution`. The position-aware production
contract and performance gates remain incomplete.


Selected materialized comparisons now use a published operand register
directly for an immediate compare or a zero test; carried values retain their
checked load path. Native execution coverage checks signed conditions and
integer, long and reference widths with spilled operands and both low and
extended resident registers, including the emitted instruction shape.
The six selected-allocation tests passed. A 12-case mode/collector/eager-deopt
probe matched HotSpot, although it does not prove every deoptimization edge.
The full serial JIT suite again completed with 7,634 passed, 85 failed and 8
ignored; its failure names are identical to the preceding run.

A two-process-per-cell interleaved screen comparing separately built binaries
measured default compile at 3,597.83 to 3,592.79 instructions/op and
compatible compile at 3,570.24 to 3,568.33; matches and groups were nearly
unchanged. This is a small diagnostic result, not a retirement-gate pass.
Evidence, binary hashes, independent counter verification and the full suite
are under `location-census-20261108/selected-cmp-in-place`.


Selected materialized comparisons can now write their boolean result directly
to an assigned GPR and publish it using the existing home and carry rules.
The native comparison test executes both low and extended registers, including
a result aliasing an input, six conditions, integer/long/reference operands
and the stack fallback. All six selected-allocation tests pass. A 12-case
mode/collector/eager-deopt probe matches HotSpot; the full serial JIT suite
remains at 7,634 passed, 85 failed and 8 ignored, with identical failure names.

A two-process-per-cell interleaved A/B screen measured default compile at
3,591.79 to 3,582.79 instructions/op and compatible compile at 3,568.88 to
3,560.37. Matches was unchanged; groups increased by about 5 instructions/op
in both modes (roughly 0.03%). These are small effects, not the retirement
gate. Raw windows, binary hashes and the independent reader are under
`location-census-20261108/selected-cmp-result`.


The debug census now records each scheduled block and its terminator or
fall-through edge, including the edge's phi-copy and poll emission. In a new
address-verified parser capture, `Pattern.compile()V` executed 1,224
instructions: 970 in data-node arms, 100 in terminator/edge ranges, 73 in
other block code and 81 outside scheduled blocks. The edge total includes
49 in fall-through edges, 37 in `If` edges and 14 in the return range.
These are emission-range counts, not yet a breakdown of individual phi
copies, polls, prologues or stubs, and do not prove removable cost. The
reader checks range overlap and containment; eight focused reader tests pass.
Build, raw trace and verified attribution are in
`location-census-20261108/block-edge-census`.


The x64 allocation harnesses now publish their synthetic thread through the
TLS mirror used by generated code; six previously failing tests pass. Four
allocation-prefetch tests now explicitly compare the disabled and enabled
settings and check the current enabled default. The full serial release JIT
suite completes with 7,644 passed, 75 failed and 8 ignored; no new failure
names appeared relative to the previous run. This is not a passing correctness
gate. Evidence is under `location-census-20261108/prefetch-test-default`.

The sampled hottest fall-through edge executes a five-instruction safepoint
poll, followed by two frame loads, one frame store and a jump, three times.
The reloads cannot be removed solely from this trace: their validity across
all incoming paths, collector moves and deoptimization must be established.


Three register-allocation tests now specify the configurations they actually
exercise: the legacy deferred-carry and kept-home phi tests explicitly disable
the allocator, and the effective-switch test checks the allocator's current
enabled default as well as its kill switch and retry refusal. The release JIT
suite now completes with 7,648 passed, 72 failed and 8 ignored, with no new
failure names relative to the preceding suite. The 72 failures remain open.
Logs are under `location-census-20261108/allocator-default-tests`.


The publish/reload retry test now pins the allocator off to exercise its
legacy sequence, while the allocator loop test compares the current default
with explicit enablement and separately executes the disabled and partial
configurations. The serial release suite now reports 7,650 passed, 70 failed
and 8 ignored, with no new failure names. Logs are under
`location-census-20261108/allocator-default-tests2`; the correctness gate
remains open.


With reference residency enabled, a proven compact receiver's plain reference
load can now write its assigned GPR directly while preserving the frame home.
Colored barriers, volatile loads, carried or split-managed values, dropped or
lazy homes, and a result aliasing the receiver keep the existing route. Both
the inline helper and deferred cold tail still join through the common
publication point. A native test executed low and extended register loads,
including aliases, and the full serial release JIT suite reported 7,651 passed,
70 failed and 8 ignored with no new failure names. The 12-case mode/collector/
eager-deopt probe matched HotSpot; that is focused coverage, not a complete GC
proof.

In two interleaved repetitions per cell with reference residency explicitly
enabled on both binaries, default compile fell from 3,637.14 to 3,618.29
instructions/op, matches from 12,249.64 to 12,161.79, and groups from
17,861.54 to 17,664.02. Compatible-mode changes were 3,612.87 to 3,593.88,
12,252.22 to 12,166.20, and 17,529.14 to 17,335.14 respectively. A separate
instruction trace confirms direct field-to-register loads execute in the parser.
Reference residency is opt-in and these numbers do not satisfy the original
shipping-default gates. Evidence is under
`location-census-20261108/ref-direct-load`.


The assigned-register result path now also covers general plain reference
field reads: compact and legacy arms write the same destination, the legacy
arm retains its tag check, and the helper and deferred tail join through that
destination. Referent and colored-barrier cases keep the prior path. Native
execution checked nine receiver/result combinations and invalid tags; the
12-case mode/collector/eager-deopt probe matched HotSpot. The serial release
JIT suite reported 7,652 passed, 70 failed and 8 ignored, with unchanged
failure names.

With reference residency enabled, a two-repetition screen measured default
matches at 12,161.75 to 12,153.81 instructions/op and compatible matches at
12,166.20 to 12,158.19; compile was essentially unchanged. Default groups
changed from 17,664.00 to 17,633.98 with a 48-instruction spread in the new
arm, and compatible groups from 17,334.26 to 17,328.25. These are exploratory
opt-in measurements, not the retirement gate. Evidence is under
`location-census-20261108/ref-direct-general`.


The current attempt additionally routes checked plain full-width reference
array reads into an assigned GPR when reference residency is enabled and
the result has a retained home. Narrow oops, armed load barriers, carried or
split-managed values, and address-register aliases retain the existing path.
A native address/result test passed; the serial release JIT suite recorded
7,653 passed, 70 failed and 8 ignored before integration with later `dev`.
The issue and the broader position-aware architecture remain open. Initial
evidence is under `location-census-20261108/ref-array-direct`.

After integration with `dev` at `77115a4e0d`, the release launcher built and
all 12 combinations of execution mode, collector and eager-deoptimization
setting in the existing stress probe matched HotSpot. The full serial release
JIT suite now passes: 7,724 tests passed, zero failed and eight were ignored.
The 70 earlier failures were addressed by updating test fixtures for current
switch defaults and optimizer behavior, preserving explicit disabled-path
coverage, and refreshing source-level emission inventories. A focused optimizer
shape test covers the multiply rewrite separately from the executed arithmetic
checks. The logs and probe output are under
`location-census-20261108/integrated-dev-final` and
`location-census-20261108/fix-suite-round8`.

This closes the JIT suite failures, not the library-performance issue. The
reference-residency improvements remain opt-in; no shipping-default result has
met the 3x row gate or established the approximately 2x architectural target.
The position-aware architecture and end-to-end throughput gap remain open.

**2026-11-13 policy correction:** `SP_BRANCH_PROFILE` is opt-in again after
[controlled arithmetic measurements](../../feature-designs/README.md)
isolated a 6.65–8.09× default regression from its atomic taps. The eleven-switch
results below are historical profiler-ON measurements; enable
`CRATONVM_JIT_SP_BRANCH_PROFILE=1` to reproduce that policy. The other ten
switches are unchanged by this correction, and this issue's gates remain open.

## 2026-11-09 shipping-policy flips: eleven switches default ON

Source `8b3fe3b5f6` (dev `644da6c174` merged), release CLI SHA-256
`f104c33b4986d96535a5791e41c83b40bc976da0fa46b72a30cffbb4089fbec1`, built with
the shipping profile and run with **no** `CRATONVM_*` overrides. The earlier
"experimental policy" is now the default, so the opt-in diagnostics above no
longer need env flags to reproduce. `=0` restores each switch.

| Wave | Switches (all `CRATONVM_JIT_...`) |
|---|---|
| 1 | `IR_CHAIN_BIMORPHIC_LINKS`, `SP_BRANCH_PROFILE`, `IR_SPLICE_COLD_ARM_EDGE_SET`, `ZGC_LEAN_START_BIT`, `IR_FUSED_CLASS_TEST_BRANCH` |
| 2 | `IR_CHECKCAST_PROXY_WAYS`, `IR_NESTED_EXACT_YIELDS_TO_PROFILE`, `ZGC_NEWARRAY_SIZE_OFF_CHAIN`, `IR_LOOP_PHI_RESIDENCY`, `IR_TRUSTED_REF_GETFIELD`, `IR_ANCHOR_TRAP_FREE_SKIP` |

Selection: a stacked A/B of the doc's opt-ins, then leave-one-out (wave 1:
`CHAIN_TREE_MAX_BYTES=650` and `INLINE_BUDGET=2600` carried nothing and stay
off), then a screen of 429 opt-in JIT switches over `compile` and `regex-matches`
(deterministic counts, one repetition each), then single-flag and combined runs over
six and then all fourteen rows. The two-row sweep alone was misleading:
`IR_CLASS_WORD_SINGLE_TEST` (-168 / -401 on compile / matches) put `matchonly`
at 3.18x, `IR_CHAIN_ENTRY_SPLICE` put `regex-find-email` at 3.45x, and
`IR_POLL_GC_POINT_CLEARS` put `split-ws` at 3.75x, so those stay off. A second
sweep over the new default (409 switches) found no further single switch worth
more than 18 instructions on `compile`.

Final instruction matrix, instructions/op, main thread, two interleaved
repetitions, serial C1/C2 workers, no dirty or failed window in any cell
(`hs` = HotSpot 25). Gap cut = 1 - (final - hs) / (dev - hs):

| Row | hs | dev tip default | final default | final compatible | gap cut (default) |
|---|---:|---:|---:|---:|---:|
| `compile` | 804 | 3,585 (4.46x) | 2,994 (3.72x) | 2,974 (3.70x) | 21% |
| `regex-matches` | 3,798 | 12,087 (3.18x) | 10,218 (2.69x) | 10,220 (2.69x) | 23% |
| `regex-groups` | 5,750 | 17,515 (3.05x) | 15,387 (2.68x) | 15,387 (2.70x) | 18% |
| `matchonly` | 7,019 | 20,774 (2.96x) | 19,508 (2.78x) | 19,507 (2.78x) | 9% |
| `regex-find-ci` | 7,330 | 20,816 (2.84x) | 19,880 (2.71x) | 19,880 (2.71x) | 7% |
| `regex-find-email` | 9,679 | 26,451 (2.73x) | 24,600 (2.54x) | 25,023 (2.59x) | 11% |
| `regex-split` | 31,662 | 76,160 (2.41x) | 69,045 (2.18x) | 69,102 (2.18x) | 16% |
| `split-ws` | 12,596 | 34,642 (2.75x) | 31,418 (2.49x) | 31,486 (2.50x) | 15% |
| `cached` | 11,773 | 27,999 (2.38x) | 24,898 (2.11x) | 25,639 (2.18x) | 19% |
| `split-comma` / `-10` | 2,832 / 2,763 | 4,626 / 4,311 | 4,234 / 3,920 | 4,229 / 3,916 | 22% / 25% |
| `regex-replaceall`, `regex-string-matches`, `list` | | 1.87-1.95x | 1.82-1.94x | 1.82-1.94x | 0-5% |

**Thirteen of fourteen rows are now at or below 2.8x in both modes; `compile`
is the one row over 3.0x (3.72x default, 3.70x compatible).** The exploratory
goal of cutting the gap by 40% was **not** reached: the cuts above are 21% on
`compile`, 23% on `regex-matches` and 18% on `regex-groups`. This is still
one measurement session on one host, so it is not the repeated-window final
gate this page asks for.

Correctness before and after the flips (HotSpot 25 as the oracle):
`R24RegexSplitRows facts` and `J27DeoptRows` matched in all 24 cases
(default / compatible x G1 / ZGC / Generational x eager chain and guard exits
0 / 1, with `CRATONVM_DBG_GC_AT_BACK_EDGE=211:7` forcing collections) for the
wave 1 binary and for wave 2 with its switches enabled by environment; the final
binary again 24/24 with no overrides; of the 476 `tools/probes/jit` probes with
a `main`, 376 (default) and 370 (`--compatible`) print the same output as the
dev-tip binary once timing lines are removed (92 print run-to-run noise, 9 do not
compile, 5 time out; the 6 and 2 raw differences are `time ...` lines only).
Unit tests: `cratonvm-types` and the full serial `cratonvm-jit --lib`
run (7,724 passed, 0 failed, 8 ignored). Fourteen fixtures needed updating
after wave 1 (five-word fake announce tables read `short_blocked` out of
bounds under the lean store; four tests asserted the old defaults) and eleven
after wave 2 (fixtures that read trap-free anchored deopt points, or the old
allocator spill policy, now pin the old shape explicitly).

Integration tests: of the 44 `vm/tests` targets named `jit_*`, `deopt_*` and
`osr_*`, 41 pass. Three fail four tests
(`jit_local_exception_handler_tests` x2, `jit_inherited_callee_constant_pool`,
`jit_npe_message_from_an_optimizing_body`); they fail the same way with all
eleven switches set to `=0`, so they are not caused by the flips, but they were
not run at the original dev sha and a fifth test in the first target is
flaky. They need an owner.

Where `compile` still spends its instructions (one traced invocation of the
final binary, 2,849 instructions; `Pattern.compile()V` is 1,225 of them, HotSpot's
whole parser chain is 753): allocation sequences are about 31% of that body
(`NewArray` 212 and `New` 164, roughly 41 per instance `new` and 56 for an
`int[]`, of which the ZGC start-bit store is about 22 and three `prefetchw`
more), reference stores 133 and calls 142. Closing the remaining ~520
instructions to 3.0x needs a change at that level (the start-bit protocol,
frame size and call sequences), not another switch.

Not done, and why this page stays open: the charset/small-call and suite
workload measurements (item 2), strings G1-G5 (item 3), the ten-process startup
stability check on the final binary (item 4), the repeated 14-row gate with
`compile` at or below 3.0x (item 5), and a
moving-GC/deoptimization audit of every edge for the three switches that
trust invariants (`IR_TRUSTED_REF_GETFIELD` drops the alignment and region
compares for `Ref`-typed receivers; `IR_ANCHOR_TRAP_FREE_SKIP` removes frame
states at nodes that cannot deopt; `ZGC_LEAN_START_BIT` gates the start-bit
store on `short_blocked`). The focused cases above passed, which is not a proof
of those edges. Raw logs, scripts and binaries are under
`/root/regex-split-evidence-20261107/register-aware-path/jit-flips-20261109/`
on the build host.

## 2026-11-09 late LICM over the predicated regex loops

Goal set for this round: every row but `compile` below 2.0x HotSpot.

**Where the instructions went.** A perf + jitdump profile joined to the IR
location census (`CRATONVM_DBG_JITC` + `CRATONVM_DBG_IR_GRAPH` byte ranges per
node) of `matchonly` put 63% of the row in `Pattern$Start.match` (the greedy
body spliced in), and within it about 35% frame-home traffic (20% home stores,
12% reloads), about 19% field-read guards (compact-header test 9%, null tests
6%, epoch and read-bounds checks 4%), 11% safepoint polls and 12% prologue. The
outer position loop re-read `matcher.to` and the greedy node's `cmin`, `next`
and `predicate` (with their guards) on every position: LICM ran before the
loop-entry class / `charAt` predicates folded the splice class tests and cut
their miss-arm calls, and no LICM ran after them.

**Change** (both default ON, `=0` kills):

* `CRATONVM_JIT_IR_LATE_LICM`: after the predicates, one more read hoist over
  the predicated loops. Each hoisted read is a new node at the end of the
  pre-header, so it runs after the predicate guards (a block orders impure
  nodes by id); only for a base proven non-null there (a dominating class
  guard, field store or used field read) and never for a base the body
  type-tests. The `charAt` entry bound runs again after it, since the hoisted
  `to` is the invariant bound of `i < to`.
* `CRATONVM_JIT_IR_LICM_FIELD_DISJOINT`: the restricted read hoist admits a
  guarded loop whose only writes are plain field or primitive element stores
  (`matcher.hitEnd = true` here) and keeps only the reads no body write may
  alias.
* The existing `PUBLISH_RELOAD_ELIDE` skips a home reload right after its own
  store for any register, not only RAX.

Same-binary A/B (one build, switches off / on), instructions/op, main thread:

| Row | HotSpot | off | on | ratio on |
|---|---:|---:|---:|---:|
| `matchonly` | 7,019 | 19,508 | 16,175 | 2.30x |
| `cached` | 11,773 | 24,945 | 21,498 | **1.83x** |
| `split-ws` | 12,596 | 31,387 | 28,054 | 2.23x |
| other 11 rows | | | | within noise (`find-email` is bimodal, 24,880 / 25,162, in both arms) |

Final shipping build (fat LTO, dev `2afd3b30cf`, no overrides) against the
LTO dev-tip binary, two interleaved repetitions, serial workers, no dirty or
failed window:

| Row | HotSpot | dev tip | final default | final compatible | ratio (default) |
|---|---:|---:|---:|---:|---:|
| `compile` | 803 | 2,993 | 2,993 | 2,974 | 3.73x |
| `regex-matches` | 3,798 | 10,220 | 10,278 | 10,292 | 2.71x |
| `regex-find-ci` | 7,330 | 19,879 | 19,881 | 19,882 | 2.71x |
| `regex-groups` | 5,730 | 15,387 | 15,346 | 15,346 | 2.68x |
| `regex-find-email` | 9,679 | 25,020 | 24,909 | 24,756 | 2.57x |
| `matchonly` | 7,019 | 19,507 | 16,174 | 16,174 | 2.30x |
| `split-ws` | 12,596 | 31,512 | 28,125 | 28,150 | 2.23x |
| `regex-split` | 31,662 | 69,000 | 69,006 | 69,096 | 2.18x |
| `regex-string-matches` | 12,887 | 24,948 | 24,948 | 24,948 | 1.94x |
| `regex-replaceall` | 28,100 | 53,785 | 53,840 | 53,932 | 1.92x |
| `cached` | 11,773 | 24,938 | 21,621 | 21,900 | **1.84x** |
| `list` | 1,817 | 3,312 | 3,313 | 3,313 | 1.82x |
| `split-comma` / `-10` | 2,831 / 2,764 | 4,232 / 3,920 | 4,231 / 3,918 | 4,228 / 3,914 | 1.49x / 1.42x |

(A no-LTO candidate against this LTO dev binary shows `regex-replaceall` +19%
and `regex-string-matches` +31%; both rows are runtime-helper heavy and are
equal to the dev tip once both binaries are LTO builds, as above.)

Correctness on the final binary with no overrides: the 24 `R24RegexSplitRows`
/ `J27DeoptRows` cases (both modes, three collectors, eager deopt 0/1, forced
back-edge GC) match HotSpot; the `tools/probes/jit` battery against the dev-tip
binary is 363 same / 94 noisy / 9 no-compile / 4 slow / 6 different in both
modes, the six being four `time ...`-only differences and two probes that hit
the 45 s battery timeout under load and finish with the same output on both
binaries when rerun alone (`R17Ffm3EintrRace`, `R21Fz10HiddenCastMsg1`). The
LTO build again matches HotSpot in all 24 cases. Unit tests: the full serial
`cratonvm-jit --lib` run passes (7,727, 0 failed, 8 ignored; three new tests
for the field-disjoint hoist, the copy's rewiring and the guard-based null
proof); `cratonvm-types` passes except tests already red on dev from other
work (three undeclared flag reads, `ARCHITECTURE.md`'s crate count).

**Still above 2.0x and why** (what each needs, not done here):

* `regex-find-ci` (2.71x): `Branch.match` loops over `atoms[]`; its `SliceI`
  receiver is an array element, so no loop-entry class predicate applies and
  the miss-arm call keeps every enclosing loop's reads in place. Needs
  trap-on-miss for monomorphic sites (`IR_SPLICE_NESTED_PROFILE_TRAP` exists
  but costs +20-60% on every row today, because it disables the `charAt`
  hoists).
* `regex-find-email` (2.6x): the `[\w.+-]` union predicate nests a second
  `CharPredicate.is` miss arm; `w7d` refuses both loops (each round needs the
  other level folded first). Needs a multi-level field-class plan.
* `regex-groups` (2.67x), `regex-matches` (2.7x): the greedy body is a real
  call on the hot path (not a miss arm), so the per-call prologue (17 32-byte
  frame-clear stores and five callee-saved saves in
  `BmpCharPropertyGreedy.match`) and two `groups` fill loops of 20 ints per
  `Matcher` dominate. Needs deeper inlining and a vectorised fill.
* `matchonly` (2.30x), `split-ws` (2.23x), `regex-split` (2.18x): the inner
  per-character loop still pays write-through home stores for single-use
  temporaries and a 5-instruction poll; closing them needs the register-named
  deopt state the site-naming work started, or a per-thread poll byte.

## 2026-11-10 fused branches and guards, allocation proofs, the copy tail

**Where the instructions went** (`CRATONVM_DBG_JITC` `ir-home-why` /
`ir-home-keep` censuses, `CRATONVM_DBG_PROVEN_FIELD_SITES`, and the perf +
jitdump join of `/root/jjp2/annot.py`):

* `Start.match`'s greedy loop materialised its `ctype[ch] & SPACE` compare
  (`SETcc; MOVZX; store; reload; TEST`) on every character: the charAt hoist's
  per-character `Guard(0 <= i)` sat between the compare and its `If`, so the
  compare's inputs' home colour was reused and the fusion refused it.
* Each call of `Start.match` (about 19 per op) paid three loop-entry class
  predicates as `ExactClassIs` -> 0/1 -> home store -> reload -> guard, and
  two identical String-class guards (the class predicate's and the charAt
  predicate's) kept one of those tests from fusing at all.
* `measure`'s inlined `find` / `search` read and wrote the fresh `Matcher`
  with a per-access compact-header test and layout-epoch compare: the `New`'s
  proofs covered only its own block, because the allocation sits in a loop.
* Every `substring` / `copyOfRange` / `toString` copied the bytes after its
  qword loop one per eight instructions.

**Changes** (all default ON or under existing switches; see the commits):

* Schedule: a single-use, frame-unnamed compare whose only reader is its
  block's `If` goes last in the block (step 5b, under
  `CRATONVM_JIT_IR_PAIR_OPERANDS`). The and-zero folds now refuse an `x` the
  `And` had absorbed (a folded load), which this exposed.
* A `Guard` on a single-use `ExactClassIs` branches on the class test's flags
  (`CRATONVM_JIT_IR_FUSED_CLASS_TEST_BRANCH`, as the `If` arm already did).
* Identical guards (same bci, inputs, frame contents, predicate kind) fold
  into one; the unread one goes, its unclaimed snapshot is dropped.
* `ir_check_elim`: an access whose bounds check ran proves a later
  `Guard(i >= 0)` on its index (`CRATONVM_JIT_IR_GUARD_FACT_ELIDE`).
* A `New`'s compactness and layout proofs cover every block it dominates,
  loops included (it is laid out by the body's own allocation on every trip).
* A caller-saved republish right after the value's own home store is skipped
  (`CRATONVM_JIT_IR_PUBLISH_RELOAD_ELIDE`).
* `emit_copy` finishes its 0..7-byte tail in one 4/2/1-byte step each.
* A vector pre-loop with fewer than one step left (`Matcher.search`'s 2..6-int
  `groups` fill, once per `find`) jumps straight to the scalar loop before its
  per-array tests, broadcasts and poll set-up.

Measured and not taken: turning off the nested hot-cap leaf rule
(`CRATONVM_JIT_IR_NESTED_GUARDED_HOT_CAP_LEAF=0`) inlines `Start.match` into
`measure` and costs `matchonly` +32%, `split-ws` +19%, `find-email` +35% (the
standalone compile is the better-optimised one); `CRATONVM_JIT_IR_PAGE_POLL=1`
and `CRATONVM_ZGC_JIT_SHORT_ANNOUNCE=1` are flat (within 0.6%); giving every
loop whose pre-header may trap the late round's copy hoist (so the `groups`
fill's field and length reads would leave the loop) costs `regex-split` +2.4%
and `split-ws` +1.5%.

Final shipping build (fat LTO, this round's tip) against the LTO dev-tip binary
of the previous round, two interleaved repetitions, serial workers, no dirty
or failed window, instructions/op on the main thread:

| Row | HotSpot | dev tip | final default | final compatible | ratio (default) |
|---|---:|---:|---:|---:|---:|
| `compile` | 803 | 2,993 | 2,980 | 2,961 | 3.71x |
| `regex-find-ci` | 7,330 | 19,882 | 19,491 | 19,490 | 2.66x |
| `regex-matches` | 3,795 | 10,305 | 9,468 | 9,454 | 2.49x |
| `regex-groups` | 5,820 | 15,346 | 13,478 | 13,479 | 2.32x |
| `regex-find-email` | 9,679 | 24,460 | 22,066 | 22,204 | 2.28x |
| `split-ws` | 12,596 | 28,076 | 25,932 | 25,939 | 2.06x |
| `regex-split` | 31,662 | 69,220 | 64,055 | 64,619 | 2.02x |
| `regex-string-matches` | 12,879 | 24,948 | 24,948 | 24,948 | 1.94x |
| `regex-replaceall` | 28,102 | 53,941 | 53,920 | 53,952 | 1.92x |
| `matchonly` | 7,019 | 16,174 | 13,347 | 13,347 | **1.90x** |
| `list` | 1,817 | 3,313 | 3,185 | 3,185 | 1.75x |
| `cached` | 11,773 | 21,568 | 19,351 | 19,333 | 1.64x |
| `split-comma` / `-10` | 2,832 / 2,763 | 4,231 / 3,918 | 3,908 / 3,858 | 3,904 / 3,854 | 1.38x / 1.40x |

Same-tree no-LTO A/B per change (each against the one before it): the
compare sink with the and-zero fix took `matchonly` 16,175 -> 14,962 and
`groups` 15,348 -> 14,354; the class-test guard fusion `find-email` -3.3%;
the guard fold and the bounds-check proof `matchonly` 14,752 -> 14,334
(`find-ci` +0.5%: the fused String test moves one register choice in
`Start.match`); the `New` layout proof `matchonly` 14,334 -> 13,700, `groups`
and `matches` -1.9%; the copy tail `split-comma` -6.3%, `cached` -2.6%,
`split-ws` -1.6%, `regex-split` -1.4%.

The short-trip vector exit, last, against the copy tail: `matches` -3.1%,
`matchonly` -2.6%, `cached` -1.5%, `split-ws` -1.3%, `groups` -1.1%.

Correctness: the 24 `R24RegexSplitRows` / `J27DeoptRows` cases match HotSpot
on the no-LTO candidate and on the LTO build, before and after the short-trip
exit. The `tools/probes/jit` battery against the dev-tip no-LTO binary, final
tree: default 369 same / 90 noisy / 9 no-compile / 6 slow / 2 different,
compatible 368 / 90 / 9 / 6 / 3, every difference a `time ...` line only (the
tree before the short-trip exit: 370 / 89 / 9 / 6 / 2 and 366 / 93 / 9 / 6 /
2, the one non-timing difference `R25LockAttribution`'s `info
waitingSamples=` sample count, `R25-PASS` on both).
`cratonvm-jit --lib`: 7,732 passed, 0 failed (new tests:
`branch_compare_goes_last_unless_a_frame_names_it`,
`a_class_predicate_guard_branches_on_the_class_test_flags`,
`identical_guards_fold_into_one`,
`a_passed_bounds_check_proves_the_non_negative_guard`,
`emit_copy_copies_exactly_n_bytes`).

**Still above 2.0x and why** (what each needs, not done here):

* `split-ws` (2.08x), `regex-split` (2.06x): each op compiles a pattern
  (`compile` is 3.71x on its own and about 11% of `split-ws`), dominated by
  allocations (about 45 instructions per `new`, a third of them the ZGC
  object-start bit) and reference-store barriers; every `substring` pays two
  such allocations. `regex-split` also shares one `Start.match` between two
  patterns: its `next.match` site is bimorphic (both classes spliced), so the
  loop-entry class predicate, which needs one class, does not apply. Needs
  per-class loop versioning, or a cheaper allocation fast path.
* `regex-find-email` (2.32x): still the nested union-predicate miss arm (see
  the previous round), and field reads of the predicate lambdas that keep
  the full compact-test sequence (`CRATONVM_DBG_PROVEN_FIELD_SITES`
  `arm-shape`, 37 sites).
* `regex-groups` (2.39x), `regex-matches` (2.59x): the greedy body is still a
  real call per match attempt; `Matcher.search`'s `groups` fill runs the
  vector pre-loop's setup and a polling scalar loop for 2..6 elements (the
  field and length reads are not hoisted: the loop is a hard-barrier loop the
  restricted read hoist does not take).
* `regex-find-ci` (2.67x): unchanged cause (the `atoms[]` loop's array-element
  receiver).
* Not taken, measured: `CRATONVM_JIT_IR_NESTED_GUARDED_HOT_CAP_LEAF=0`
  (inlines `Start.match` into `measure`: `matchonly` +32%, `split-ws` +19%,
  `find-email` +35%), `CRATONVM_JIT_IR_PAGE_POLL=1` and
  `CRATONVM_ZGC_JIT_SHORT_ANNOUNCE=1` (flat).

## 2026-11-10 (late) memory-phi frame words, the masked fill tail

**Where the instructions went** (`ir-frame-clear` / `ir-frame-anatomy`
censuses, the perf + jitdump join):

* Every body's fresh-frame clear zeroed the homes of its memory phis. A memory
  phi has a colour (every phi wants a slot) but no machine value: the edge
  copies drop it, and no OSR seed or snapshot names it, so the clear was the
  only writer of that word.
* `Matcher.search`'s `groups` reset (2..6 ints, once per `find`) left the
  vector pre-loop's early-out for the polling scalar loop every time: there is
  never a full 16-lane step.

**Changes** (default ON):

* A pinned memory phi's word joins the per-body claim (`claimable_pinned_nodes`)
  and so leaves the fresh-frame clear.
* An AVX-512 broadcast fill (`a[i] = inv`, one base, no reduction) with
  fewer than one step left stores the remaining 1..15 lanes under a `K1` mask
  built from the count, then leaves with the induction variable at `n`. The
  early-out to the scalar loop is kept for every other plan.

Same-tree no-LTO A/B, each against the one before it: the memory-phi claim
`matchonly` -0.3%, `groups` -0.5%, `split-ws` -0.6%, `cached` -0.4%; the masked
tail `groups` -5.6%, `matchonly` -2.6%, `cached` -2.1%, `matches` -1.8%,
`regex-split` -1.8%, `split-ws` -1.3%.

Measured and not taken: also leaving an empty fill (`n - i == 0`) before the
base tests is flat or mixed (`regex-split` +1.0%, `split-ws` +0.3%, `cached`
+0.5%, `matches` -1.0%).

Also measured and not taken: `ArrayList.grow`'s own compile prunes
`Arrays.copyOf(T[],int)`'s nested bodies to a call, because the VM files
late-static-cell metadata for the native `Array.newArray`, which the cell admitter
then refuses, so `Array.newInstance`'s body is refused only at the append and its
chain goes with it. Filing that metadata only for a callee with bytecode makes
`grow` splice `copyOf` (`Array.newInstance` takes the never-run exit), but it
is flat: `split-ws` 25,381 -> 25,421, every other row within 0.1% except
`find-email` (-2.5%, within its run-to-run spread). The steady-state profile's
6% for `grow` + `copyOf` is mostly the copy, not the call.

A baseline note: the host tree must carry the whole dev diff from the r25
base. A tree missing part of it read `matchonly` 16,221 for a binary that
measures 13,348, so every number here is same-tree.

Final shipping build (fat LTO, this round's tip) against the LTO tip of the
previous round, two interleaved repetitions, serial workers, instructions/op:

| Row | HotSpot | previous tip | final default | final compatible | ratio (default) |
|---|---:|---:|---:|---:|---:|
| `compile` | 803 | 2,980 | 2,950 | 2,932 | 3.67x |
| `regex-find-ci` | 7,330 | 19,491 | 19,388 | 19,387 | 2.65x |
| `regex-matches` | 3,795 | 9,468 | 9,191 | 9,200 | 2.42x |
| `regex-find-email` | 9,679 | 23,344 | 23,073 | 21,936 | 2.38x |
| `regex-groups` | 5,766 | 13,478 | 12,657 | 12,658 | 2.20x |
| `split-ws` | 12,596 | 25,908 | 25,378 | 25,448 | 2.01x |
| `regex-split` | 31,668 | 64,010 | 63,120 | 63,102 | **1.99x** |
| `regex-string-matches` | 12,887 | 24,948 | 24,948 | 24,948 | 1.94x |
| `regex-replaceall` | 28,064 | 53,939 | 53,916 | 53,940 | 1.92x |
| `matchonly` | 7,018 | 13,348 | 12,964 | 12,964 | 1.85x |
| `list` | 1,817 | 3,185 | 3,183 | 3,183 | 1.75x |
| `cached` | 11,773 | 19,354 | 18,946 | 18,875 | 1.61x |
| `split-comma` / `-10` | 2,832 / 2,764 | 3,908 / 3,858 | 3,893 / 3,842 | 3,889 / 3,838 | 1.37x / 1.39x |

`regex-find-email` moves about ±3% between runs of the same binary (the
previous tip read 22,066 last round and 23,344 here).

Correctness: the 24 `R24RegexSplitRows` / `J27DeoptRows` cases match HotSpot
on the no-LTO candidate and on the LTO build. The `tools/probes/jit` battery
against the previous no-LTO candidate: default 366 same / 93 noisy / 9
no-compile / 5 slow / 3 different, compatible 367 / 90 / 9 / 6 / 4. Every
difference is a `time ...` line, except `R17Ffm3EintrRace` in compatible mode:
the candidate's run passed the battery's 45 s timeout (`-999`). Rerun alone,
interleaved with the previous candidate, it prints `PASS` in 25-26 s on both
binaries (twice each).
`cratonvm-jit --lib`: new test `masked_fill_tail_writes_exactly_n_lanes`
(every count below the lane width, int and long, with guard words on both
sides); the updated `r25w2_a_phi_is_claimable_only_when_no_reference_can_reach_it`
covers the memory phi.

**Still above 2.0x:** `split-ws` (2.01x), `regex-groups` (2.20x),
`regex-find-email` (2.38x), `regex-matches` (2.42x), `regex-find-ci` (2.65x),
and `compile` (3.67x, outside the goal). The causes recorded in the previous
section still hold; `split-ws` additionally pays about 100 instructions per op
for `Pattern.split`'s `localsPos` reads, whose `New` proof is refused because
an OSR entry bypasses the allocation (`field_proof_reaches`).

## 2026-11-10 (evening) field proofs through spliced loops, per-trip receivers and loaded references

**Where the instructions went** (perf + jitdump + IR join on `find-ci`, then
the `proven-receiver-fields` census and a temporary per-site trace):

* `find-ci` spent 83% in one `Start.match` body (Branch and SliceI spliced).
  Every field read in its loops took the establish or FULL form: the layout
  epoch compare, a null test and the compact test before the load, about 7
  instructions over the load itself, about 11 reads per text position.
* The `matcher` and `this` proofs put down in block 0 never reached the
  spliced `Branch`/`SliceI` loops: `field_proof_entry_blocks` listed every
  spliced loop header as an OSR entry, and no local names the receiver there.
  A spliced header's pc is at or past the method's code length, and the OSR
  door enters only at a backward-branch target of the method's own code, so
  no stub there is ever asked for.
* A receiver loaded each trip (`this.next`, `atoms[n]`) re-ran its establish
  test in every block, because cross-block proofs required a value defined
  once per activation.
* A loop pre-header's hoists (the `charAt` hoist's `String` reads, the class
  predicates) all carry the header's pc, so the lone-pc rule kept every one
  of them FULL, once per `Start.match` call.
* `Branch.size`, the `atoms` loop's header read, established on every inner
  trip: its receiver had no site before that loop.

**Changes** (default ON, each with a test):

1. A spliced callee's pc is no OSR entry: the stub planner refuses it ("the
   door never asks for this bci") and `field_proof_entry_blocks` drops it.
2. A per-trip receiver's proof covers the blocks its guard dominates when the
   receiver's definition block dominates the guard's block: a path from its
   latest definition that skipped the guard, prefixed by an entry path to the
   definition's first visit, would skip it from the method entry. Entry
   bypasses stay rule 2's.
3. A pc shared only by field/array reads, lengths and guards in blocks on no
   cycle counts as lone (`pure_once_shared_pcs`): replaying a read commits
   nothing, and the cycle rule keeps out unrolled copies.
4. A reference loaded before a loop that re-reads its fields is tested once
   at its load (null, compact, epoch; each deopting at the load's own pc)
   under the existing co-establish switch (`coestablish_loaded_reference`).
5. A splice's return into the method's code is no backward branch: the
   bottom-test rule of `osr_entry_block_is_back_edge_target` compares a
   spliced predecessor at its invoke (`resolve_spliced_pc`). Before, the
   block after `predicate.is` in the greedy body counted as an OSR entry
   bypassing the pre-loop proof on `this`.

Same-tree no-LTO A/B, each against the one before it, three repetitions:

| Row | 1+2 (vs dev tip) | 3 | 4 |
|---|---:|---:|---:|
| `regex-find-ci` | -9.9% | flat | -3.7% |
| `regex-matches` | -3.5% | flat | +0.6% |
| `regex-find-email` | -3.9% | -0.3% | -5 to -9% |
| `regex-groups` | -4.0% | -0.5% | flat |
| `split-ws` | -1.0% | -0.5% | -0.3% |
| `regex-split` | -4.2% | -2.1% (noisy) | -0.8% (noisy) |

The other eight rows moved within 0.5% (`matchonly` -2.2%, `list` +0.6%).
Change 5, against change 4: `find-email` -1.0%, `groups` -0.9%, the other
four rows flat.

Measured and not taken:

* `CRATONVM_JIT_IR_SPLICE_NESTED_PROFILE_TRAP=1` is still a loss on every
  regex row (`find-ci` 19,388 -> 30,077, `split-ws` 25,429 -> 35,391).
* `CRATONVM_JIT_IR_REG_ALLOC` is already default ON; the frame-slot traffic
  in these loops is the allocator's write-through homes (`ir-home-why`:
  `no_reg=23` values in `Start.match`, with five callee-saved GPRs on System V).

**Next levers found, not done:**

* The getfields inside spliced lambda forwarders (`union`'s `arg$1`/`arg$2`,
  `Range`'s bounds) have no compact row: lambda proxies are VM-synthesized and
  always allocated in the legacy cell shape, so each read takes the
  legacy-only arm (null, compact, slot-count and tag tests). Hoisted copies
  inherit the missing row (`find-email`'s greedy pre-header, 8 FULL reads per
  call). A compact proxy layout, or a read arm keyed on the proven proxy
  class, would remove them.
* An establish site's explicit null test could be the compact test's own
  fault (the implicit-null table the IR page poll uses).
* Per-call entry cost of `BmpCharPropertyGreedy.match` (frame clear, five
  callee-saved saves, parameter copies, the frame record and the entry poll):
  about 60 instructions per call.

Profiles on this host put up to half of a body's samples on the instruction
after a load from freshly allocated memory (first-touch faults, PMI skid), so
the rankings above come from censuses and code reading, not sample shares.

Shipping build (fat LTO, changes 1-4; change 5 is measured no-LTO only)
against the previous LTO tip, two interleaved repetitions, serial workers,
instructions/op (SHA-256 `64e60cc51ebb9cda21e8b58758dc6297bb39796ad33d5130e367a9c9ce05e11a`):

| Row | HotSpot | previous tip | final default | final compatible | ratio (default) |
|---|---:|---:|---:|---:|---:|
| `compile` | 803 | 2,952 | 2,942 | 2,922 | 3.66x |
| `regex-matches` | 3,795 | 9,202 | 8,901 | 8,901 | 2.35x |
| `regex-find-ci` | 7,329 | 19,387 | 16,816 | 16,816 | 2.29x |
| `regex-find-email` | 9,536 | 23,214 | 20,324 | 20,766 | 2.13x |
| `regex-groups` | 5,750 | 12,657 | 12,090 | 12,089 | 2.10x |
| `split-ws` | 12,596 | 25,315 | 24,994 | 25,116 | **1.98x** |
| `regex-string-matches` | 12,846 | 24,948 | 24,948 | 24,948 | 1.94x |
| `regex-replaceall` | 28,101 | 53,882 | 53,844 | 54,070 | 1.92x |
| `regex-split` | 32,062 | 63,062 | 59,634 | 60,920 | 1.86x |
| `matchonly` | 7,019 | 12,964 | 12,558 | 12,558 | 1.79x |
| `list` | 1,817 | 3,184 | 3,184 | 3,431 | 1.75x |
| `cached` | 11,773 | 18,923 | 18,504 | 18,531 | 1.57x |
| `split-comma` / `-10` | 2,832 / 2,763 | 3,892 / 3,842 | 3,875 / 3,826 | 3,872 / 3,821 | 1.37x / 1.38x |

Correctness: the 24 `R24RegexSplitRows` / `J27DeoptRows` cases match HotSpot on the no-LTO candidate (commit 3) and on the LTO build (commit 4). The `tools/probes/jit` battery, commit 3 no-LTO against dev: default 367 same / 93 noisy / 9 no-compile / 6 slow / 1 different (an `info waitingSamples` count), compatible 365 / 90 / 9 / 10 / 2 (two `time` lines). LTO build against the previous LTO tip, default: 362 / 94 / 9 / 8 / 3: one `time` line; `R21Fz4ArithEdge1` passed the 45 s timeout under load (rerun alone: 28-43 s on both binaries, identical output); and `R22ReviewIntrinsicsEdges`, which fails intermittently on every build including the previous tip and dev before this round ([its page, now fixed](../../feature-designs/README.md)). Compatible: 365 / 91 / 9 / 8 / 3: two `time` lines, and `R22ReviewIntrinsicsEdges` failing on the previous tip this time. Compatible-mode `list` swings 2,930-3,475 on the previous tip as well (four repetitions each).
`cratonvm-jit --lib`: 7,736 passed, 0 failed (four new tests, one source pin).

**Still above 2.0x:** `regex-matches` (2.35x), `regex-find-ci` (2.29x), `regex-find-email` (2.13x), `regex-groups` (2.10x), and `compile` (3.66x, outside the goal).

## 2026-11-11 exact per-block attribution; the self-bounded fill

**Method.** `perf` on the 64.177 VM puts most samples on the instruction
after a VM exit (first touch of fresh memory, or any long stall), even in an
allocation-free loop, so sample shares cannot rank work there. A temporary
local patch (never committed) emitted `MOV R11, &counter; ADD QWORD [R11], 1`
at every IR block start, dumped the counters by `(compile, block, offset)`,
and joined them with the jitdump bodies: each block's hot path (conditional
branches not taken, jumps followed inside the block) times its count. On
`mat-reuse` the join summed to 929 instructions per `matches()` call against
the measured ~896.

**Decomposition rows** (added to the host kit only, instructions per op, 8
calls each):

| Row | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| `mat-alloc` (`WORDNUM.matcher(s)`) | 1,361 | 2,499 | 1.84x |
| `mat-reset` (`reset(s)`) | 600 | 1,726 | 2.88x |
| `mat-reuse` (`reset(s).matches()`) | 2,655 | 7,165 | 2.70x |

So `regex-matches` is mostly matching and resetting, not allocation.

**Where a `matches()` call goes** (~930 instructions, HotSpot ~330):
the caller (`reset`, `match`'s setup, the call) ~370, of which three tiny
fills (`groups` = 2 ints, `locals` = 0 on JDK 25) ~28 each; the greedy body's
entry (prologue ~38, receiver/`charAt` prerequisites ~60); and its
`[a-z]+` loop at ~50 instructions per character, of which ~23 re-read
`this.predicate`, class-test it and re-read the `Range` lambda's two captures
(legacy cells with slot-count checks) every character. The `predicate.is`
site is bimorphic (`[a-z]+` and `\d*` share the greedy body), so the
field-class pass (`w7d`, monomorphic only) refuses it and the miss arm's
call keeps every read in the loop. On `regex-groups`, `Start.match`'s
pre-header (the hoisted `next` chain and predicate class tests, each a fresh
epoch/compact establish) costs ~113 instructions per successful `find`.

**Change (default ON):** a vector loop whose bound is the length of the
array it stores into skips that array's second null test and the
`n > a.length` compare (five instructions per fill). New probe
`tools/probes/jit/r25/R25FillTailLengths.java` (every fill length 0..100 at
several offsets, whole arrays 0..40, a null array) equals HotSpot. Same-tree
no-LTO A/B: `mat-reset` -3.7%, `regex-matches` -1.1%, `matchonly` -0.7%,
`cached` -0.5%, `groups` -0.4%; the rest within spread.

**Measured and not taken:**

* A masked tail after the vector steps (for 17..31-element remainders): no
  target row has such a fill on JDK 25 (`Matcher` no longer pads `groups` to
  10 groups), flat everywhere.
* `CRATONVM_JIT=ir-nested-guarded-hot-cap-leaf=0` (splices the greedy body
  into its caller): `regex-matches` -3.7%, but `find-email` +55%,
  `split-ws` +20%, `matchonly` +38%.
* `CRATONVM_JIT=ir-splice-nested-profile-trap` with `String` bodies kept as
  guarded calls: still a loss on every row (`matchonly` +67%), so the loss
  is not the `charAt` hoists alone.

**Next levers, with their exact size:** a bimorphic field-class plan for
the greedy loop (~23 of ~50 instructions per character); `Start.match`'s
pre-header establishes (~113 per `find` on `groups`); the three small fills'
fixed vector set-up (~23 each per `matches()` call); the greedy entry (~100
per call). The per-object layout-epoch compare could become one compare per
safepoint interval only if layout replacement is proven to happen with every
Java thread stopped; class definition (`classloading/src/class.rs`) can run
on any thread, so that is not assumed.

## 2026-11-11 (later) the bimorphic greedy hoist; dev's armed load barrier

**Default collector changed under the rows.** After merging dev
(`236881b427`), the same-harness counts moved without any JIT change:
`regex-matches` 8,817 -> 13,506, `regex-find-ci` 16,788 -> 35,212,
`regex-find-email` 21,149 -> 312,833. Bisected to
`CRATONVM_ZGC_LOAD_BARRIER` (part of the default-on concurrent relocation
profile): with it off every row is back to the pre-merge count exactly. No
GC cycle runs in the measured window; compiled code calls `jit_getfield`
for field reads the armed gate closes. Details and the reproducer:
the public feature contracts
(closed: relocation is opt-in again). The A/B below uses `CRATONVM_ZGC_CONCURRENT_RELOCATE=0` on every arm.

**Tried and not taken: a bimorphic field-class plan.** `predicate.is` in
`BmpCharPropertyGreedy.match` is a two-class guarded splice (`Range` and
`ASCII_DIGIT` lambdas, `[a-z]+\d*`). The w7d pass was extended to a key
tested against two classes (the `B` test on the `A` test's miss edge, whose
miss edge calls): one entry read, `Guard(class in {A, B})`, the `A` tests
take the entry value's class bit, the `B` tests fold, the miss call goes.
It also had to be offered the class-predicated loops (here the `charAt` is
a guarded `String` splice whose test the class predicate already hoisted,
so the trap-only offer never saw the loop). It engaged on both greedy
loops, unit tests and a probe warming the pair then feeding a third and
fourth predicate class equalled HotSpot. Same tree, relocation off:

| row | base | hoist | hoist + `..._LONE_PER_FRAME=1` |
|---|---:|---:|---:|
| regex-matches | 8,817 | 9,043 | 8,806 |
| regex-groups | 11,938 | 11,938 | 12,010 |
| regex-find-ci | 16,788 | 16,788 | 16,798 |
| matchonly | 12,471 | 12,471 | 12,591 |
| split-ws | 24,986 | 24,864 | 25,104 |

Neutral at best. The in-loop `predicate` read was already a proven load and
its class test a fused compare; the entry pays a read, two class tests and a
guard per loop entry, and `[a-z]+\d*` enters its loops for a few
characters. The nested `\d*` loop's two copies share header pc 309, so their
entry reads fail the lone-pc rule and take the FULL guard (the per-frame
rule fixes that, at a small cost elsewhere). With the barrier armed the
same change wins (`matchonly` -8%, `groups` -4%) because the cut miss call
takes helper traffic with it; that is the other page's problem.

**What the per-character cost actually is.** After this, the greedy body's
remaining guard cost is the `Range` lambda's two captures (`lower`, `upper`):
VM-minted proxy, legacy cells only, so both reads take the FULL arm
(`reason=arm-shape`) every character. Their base is loop-invariant once the
predicate is read at the entry, but they sit in the `A` arm, so hoisting
them needs the loop versioned on the `A` test (the existing
`class_unswitch` is FFM-only and refuses splice frames) or a cheaper legacy
capture read. That, not the predicate read, is the next lever for
`regex-matches`.

## 2026-11-11 (evening) `Matcher.reset` attempt: late LICM, small scalar fill bypass, `String.coder` proofs

This attempt targets `mat-reset` (`Matcher.reset(s)`), where `Matcher.groups`
(2 ints) and `locals` (0 ints on JDK 25) paid fixed vector-fill setup behind a
trapping pre-header read and repeated string/field guards.

**Changes (default ON):**

* `jit/src/ir_optimize.rs`: let `CRATONVM_JIT_IR_LATE_LICM` run even when no
  loop was class-predicated (still gated on `!osr_route && !graph.osr_compile`
  and `licm_after_array_hoist_enabled()` so `CRATONVM_JIT_IR_LICM_AFTER_ARRAY_HOIST=0`
  is respected), allowing its end-of-pre-header copies to hoist invariant field
  reads (`Matcher.groups`, `locals`) past trapping pre-header nodes.
* `jit/src/ir_lower.rs` (`emit_vector_preloop`): check `bound - j <= 0` before
  per-array guards and invariant broadcasts, branching straight to a pure loop
  header's exit block (`trivial_vec_header_exit`) with no `VZEROUPPER`. For
  1-base constant-zero primitive fills (`Int`/`Long`/`Byte`/`Short`, no
  reduce/scan/iota/scoped/widen/shift) with `0 < bound - j < 4`, emit 1–3
  unrolled GPR stores (`emit_small_scalar_fill`), advance `iv = bound`, and
  jump straight to exit without AVX-512 broadcasts or `VZEROUPPER` (plus a
  direct exit edge after the `4..lanes-1` masked tail).
* `jit/src/ir_check_elim.rs` & `jit/src/ir_lower.rs` (`dominating_exact_class`,
  `emit_str_coder_load`): treat passing `Guard(ExactClassIs(x))` as proving
  `x` non-null downstream (`guard_proves_non_null_operand`) without causing a
  prior null guard to self-elide `ExactClassIs`. Use `dominating_exact_class`
  and `layout_proven`/`compact_proven` in `emit_proven_receiver_getfield` and
  `try_emit_ir_string_search_prefix` so `StrCoder`/`StrIsLatin1` skips
  redundant null/exact-class/epoch/`GC_FLAG_COMPACT` checks and drops the dead
  `String.value` load.

**Verification and measurements (non-LTO release candidate vs `l1` no-LTO, `lto13`, and HotSpot 25):**

* **Unit tests:** `cargo test --release -j 9 -p cratonvm-jit --lib` passed
  (`7743 passed; 0 failed; 8 ignored`, including new tests in
  `jit/src/ir_check_elim.rs` and `jit/src/ir_lower/epoch_param_proof_tests.rs`).
* **Correctness probes (default and `--compatible`):** `R24RegexSplitRows facts`
  (`rc=0`, identical fact hashes), `R25FillTailLengths` (`diff` vs HS25 = 0),
  and `J27DeoptRows` (`diff` vs HS25 = 0) all matched HotSpot 25 in both modes.
* **Instruction counts (`icount3.sh`, `REPS=2 SERIAL=1 QSTABLE=24`, default mode; `--compatible` `REPS=1`):**

| Row | HotSpot 25 | Before (`l1` / `lto13`) | After (no-LTO, default) | After (`--compatible`) | Delta vs `l1` |
|---|---:|---:|---:|---:|---:|
| `mat-reset` | 600 | 1,662 / 1,662 | **1,494** | **1,494** | **-10.1%** (-168) |
| `mat-reuse` | 2,655 | 7,058 / 7,058 | **6,792** | **6,795** | **-3.8%** (-266) |
| `regex-matches` | 3,798 | 8,813 / 8,810 | **8,647** | **8,675** | **-1.9%** (-166) |

* **Broader R24 screen (`ab44`, `REPS=1 SERIAL=1 QSTABLE=24`, no-LTO default vs `lto13`):**
  `matchonly` 12,401 (`lto13` 12,471, HS 7,019), `regex-find-ci` 16,511
  (`lto13` 16,788, HS 7,330), `cached` 18,817 (`lto13` 18,313, HS 11,773),
  `regex-groups` 12,089 (`lto13` 11,936, HS 5,803), `split-ws` 25,232
  (`lto13` 25,163, HS 12,596), `regex-find-email` 20,999 (`lto13` 19,954,
  HS 9,678).
* **Compiled body check (`CRATONVM_DBG_DUMP_JIT=Matcher.reset`):** both
  `Matcher.reset()` (`len=8169`) and `Matcher.reset(CharSequence)` (`len=7987`)
  emit the 6 unrolled `MOV DWORD [RAX+RDX*4+d8], R9D` stores for the `groups`/
  `locals` fills; entry layout-epoch, `GC_FLAG_COMPACT` (2 sites), read-bounds
  checks, 5 `vzeroupper` sites on vector fallbacks, static `REUSE` loads, and
  the `localsPos` / `IntHashSet.clear()` call account for the remaining ~894
  instructions/op gap vs HotSpot on `mat-reset`.

**Remaining gap for `mat-reset`:** still at `1,494` vs HotSpot `600` (`2.49x`);
reaching a >= 2x gap reduction (`<= 1,131`) needs further work on entry
layout-epoch / compact / read-bounds elision, forwarding `LoadStatic` (`REUSE`)
and its fields across non-mutating loops/calls in `join_load_forward`, and
cutting the remaining `localsPos.clear()` / entry overhead.

## 2026-11-16 Heap-specializing tier (`CRATONVM_FROZEN_PAGES`, experimental / off by default)

Quiet-host (`176.57.150.136`, Linux x86_64, `CRATONVM_BOOT_IMAGE=off`) 3-round interleaved A/B of `R24RegexSplitRows all 0.4 7` (HotSpot 25 vs `CRATONVM_FROZEN_PAGES=0` vs ON, median of last 5 reps per process, then median of 3 rounds; ns/op):

| Row | HotSpot (ns/op) | OFF (ns/op) | ON (ns/op) | OFF/HS | ON/HS | OFF/ON |
|---|---:|---:|---:|---:|---:|---:|
| `regex-find-email` | 3778.0 | 14553.0 | 9434.0 | 3.85x | 2.50x | **1.543x** |
| `regex-split` | 7054.0 | 738780.0 | 503894.0 | 104.73x | 71.43x | **1.466x** |
| `matchonly` | 2212.0 | 12658.0 | 9901.0 | 5.72x | 4.48x | **1.278x** |
| `compile` | 105.0 | 897.0 | 789.0 | 8.54x | 7.51x | **1.137x** |
| `list` | 189.0 | 2379.0 | 2242.0 | 12.59x | 11.86x | **1.061x** |
| `regex-replaceall` | 4930.0 | 10085.0 | 9702.0 | 2.05x | 1.97x | **1.039x** |
| `regex-string-matches` | 2091.0 | 5155.0 | 4973.0 | 2.47x | 2.38x | **1.037x** |
| `split-comma-10` | 389.0 | 2470.0 | 2385.0 | 6.35x | 6.13x | **1.036x** |
| `split-comma` | 382.0 | 2634.0 | 2708.0 | 6.90x | 7.09x | 0.973x |
| `cached` | 3385.0 | 16592.0 | 17218.0 | 4.90x | 5.09x | 0.964x |
| `split-ws` | 4494.0 | 18401.0 | 20523.0 | 4.09x | 4.57x | 0.897x |
| `regex-find-ci` | 1239.0 | 5167.0 | 8393.0 | 4.17x | 6.77x | 0.616x |
| `regex-matches` | 422.0 | 2727.0 | 6763.0 | 6.46x | 16.03x | 0.403x |
| `regex-groups` | 1062.0 | 5833.0 | 498623.0 | 5.49x | 469.51x | 0.012x |

Because steady-state app and R24 results are mixed (with `regex-matches`, `regex-find-ci`, and `regex-groups` showing tier-up/warmup spikes under 0.4 s × 7 wall-time batches), the heap-specializing tier is **experimental and off by default** (`CRATONVM_FROZEN_PAGES=1` / `CRATONVM_GC=frozen-pages` to opt in). Note: during the `bba7e77190` app-bench run, `frozen_pages::enabled()` had temporarily been hardcoded to `true` after flag-surface reduction, so `CRATONVM_FROZEN_PAGES=0` was not yet wired there; restoring `runtime_flag_on("CRATONVM_FROZEN_PAGES")` makes it an explicit opt-in again.

### 2026-11-16 rerun with a working switch: no gain on the app rows or the string rows

The 2026-11-14 app table (tier doc, `CRATONVM_FROZEN_PAGES=0` vs ON) was taken
while `enabled()` was hardcoded on (note above), so both arms ran the tier and
its 0.89x-1.18x spread was noise. Rerun on the 16-core host (`64.177.43.150`,
load 8-21), dev `670297786b`, one binary, tier ON vs
`CRATONVM_NO_FROZEN_PAGES=1`, `CRATONVM_BOOT_IMAGE=off`, the 13 rows'
`run-apps-benches-interleaved.sh` commands, 3 interleaved rounds (arm order
alternating), `perf stat` per arm. Wall time swung +-30 % with the load, so
the verdict is read from `instructions:u` (all threads, JIT included):

| Row | instructions ON/OFF, per round | Engagement (ON): methods sent back to refold / thaws |
|---|---|---|
| Hibernate `ElementCollection` | 1.18 1.12 1.16 | 2 180 / 3 677 (396 K objects walked) |
| Hibernate `ActionQueue` | 1.09 1.16 1.11 | 181 / 1 099 |
| Spring Boot `VirtualZip` | 1.06 1.09 1.08 | 158 / 344 |
| Spring `IntroductionBenchmark` | 1.05 1.05 1.05 | 142 / 881 |
| Tomcat `ELParser` | 1.03 1.04 1.06 | 171 / 445 |
| Commons Math `JdkMathTest` (2 s run) | 1.76 1.25 1.16 | 46 / 361 |
| BouncyCastle `AESFastTest` (1 s run) | 1.02 1.26 1.60 | 29 / 179 |
| Commons Math `JdkMathTestPerformance` | 1.08 1.02 1.01 | 204 / 355 |
| Netty `DateFormatter` | 1.01 1.01 1.04 | 154 / 749 |
| Tomcat `Response` | 1.05 1.23 0.98 | 225 / 869 |
| Tomcat `CharsetCache` | 0.97 1.03 1.00 | 164 / 497 |
| Tomcat `OneLineFormatter` | 1.00 1.00 1.01 | 170 / 454 |
| Tomcat `Method` | 1.00 2.20 1.00 | 135 / 526 |

No row runs fewer instructions with the tier on. `Method` is bistable in both
arms (4.67 T instructions / ~480 s or 2.12 T / ~105 s per run; OFF hit the
slow mode once, ON twice), which is where the old 1.15x came from.
`ELParser`'s own timing assertion (`ReInit` faster than `new`) failed once in
each arm under the load.

R24 string rows, same binary, `R24RegexSplitRows` (median of the last 5 of 7
0.4 s reps per process, median of 3 interleaved rounds). One row per process:
all 14 rows within 0.94-1.02 OFF/ON. All rows in one process (the run above):
medians 0.89-1.15 with round-to-round spread wider than that; only
`regex-groups` is slower ON in every round (0.89, 0.89, 0.93), and it is flat
on its own. The 1.5x wins and the 85x `regex-groups` loss above do not
reproduce here. `facts` equal to HotSpot in both modes, both arms. The tier
engages (26 identity guards, 42 refolds) and buys nothing measurable.

Verdict: keep it off by default. What it costs is recompile churn (every
refold is a rebuild) plus the graph walk and the thaws; what it would win
needs hot code reading stable objects, and none of these rows has that. The
small probe rows (`config`, `bean`, `lambda`) remain its only measured gains.
