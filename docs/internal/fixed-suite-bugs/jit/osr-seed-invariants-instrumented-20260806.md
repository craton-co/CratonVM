# The OSR entry-seed invariants, made checkable

**2026-08-06.** Not a bug page: the two instruments that let the OSR
entry-seed family be *measured* rather than reproduced, and the record of the
two wrong versions of the first one, because each read exactly like a finding.

## What the family is

At an OSR entry the trampoline copies `jit_locals[i]` into local `i`'s home,
**in ascending index order**, skipping any `i` set in the entry's dead mask.
Every miscompile in this family is a violation of one of two properties of that
loop:

1. **No seed lands on a live value.** If two locals it seeds share a home, the
   higher index overwrites the lower. (`Arrays.sort(double[])` / the ES-tdigest
   XMM case.)
2. **Every live local the body reads from a register gets seeded.** If a local's
   OSR assignment was stripped while `reg_for_local` still hands the body that
   register, the body reads a register nobody wrote.
   (`Arrays.sort(long[])`, `arrays-sort-long-osr-miscompile-FIXED.md`.)

Three mechanisms in `publish_entry_metadata` exist to guarantee these — the
pure-high-half strip, the per-entry-PC dead mask, the 2026-07-27 hazardous
refinement. **Nothing asserted that they succeed.** Each failure was found by
its symptom, months apart, in a different subsystem.

`CRATONVM_DBG_OSR_SEED_COLLISION=1` asks both questions directly, per entry,
over the metadata about to be published, and names the method and the local.

## Two wrong detectors first, and why each was worse than none

| version | filter | result on a **passing** `SortProbe` |
|---|---|---:|
| v1 | every basic-block start | 310 "collisions" |
| v2 | + only takeable entries | 5251 "collisions" |
| v3 | + only when the overwritten local is LIVE | **0** |

* **v1** ignored that OSR enters a *loop header* with a valid entry offset and a
  **zero** dead mask — `can_osr_enter_with` refuses a non-zero mask outright, so
  most block starts can never be entered at all.
* **v2** ignored that two **dead** locals sharing a register overwrite each
  other harmlessly. The hazardous refinement permits that on purpose; counting
  it is counting the design.

A detector that reports thousands of hits on a green run is worse than no
detector: the next reader learns to skip its output. Both versions would have
been believed if the number had been small.

Both passes now print their denominator (`[osr-seed-scan] … takeable_entries=N
live_collisions=… stripped_live=…`) because "0 hits" and "the filter excluded
everything" are otherwise the same output — and this filter has already been
wrong in exactly that direction twice.

## The positive control, without which every zero above is worthless

`CRATONVM_JIT_OSR_STRIP_ALL_HIGH_HALVES=1` restores the pre-`14a2740859` strip:
every slot the whole-method scan calls a high half loses its OSR register
assignment, including slots that are a live cat-1 local in a disjoint range.
One binary, two arms, `probes/SortProbe.java` (20000 longs, 5 reps):

| arm | probe | `[osr-seed-stripped]` | `[osr-seed-collision]` | takeable entries |
|---|---|---:|---:|---:|
| **defect on** | **`PROBE-FAIL`** — `AIOOBE: Index 581829807 out of bounds for length 20000` at `DualPivotQuicksort.mixedInsertionSort` | **51** | 0 | 48 |
| defect off | `PROBE-OK 5 sorts` | 0 | 0 | 48 |

The defect arm's exception is the `arrays-sort-long-osr-miscompile` signature
verbatim — a garbage index in the hundreds of millions with a correct length —
and the detector names the method and the local (`LIVE local 6 reads r12 in the
body but the trampoline seeds no register for it`) rather than reporting a
count.

So the instrument is red on a real miscompile and green on the repaired code,
and the 2026-08-04 fix is confirmed load-bearing rather than assumed.

## First use: the loader/zip cluster's last open item

`OriginTrackedYamlLoaderTests` (13 tests) was the page's item 4 — an OSR
miscompile with no fix, no attribution and no reproduction. Windows,
`cratonvm-yamlosr-20260806.exe`:

| arm | tests | methods scanned | takeable entries | `collision` | `stripped` |
|---|---|---:|---:|---:|---:|
| default | 13/13 | 234 | 1038 | 0 | 0 |
| `LOCAL_REGS=3` | 13/13 | 234 | 1058 | 0 | 0 |
| `STRIP_ALL_HIGH_HALVES=1` | 13/13 | 235 | 1039 | 0 | **0** |

The third row is the interesting one. That switch produces 51 violations and a
hard failure on `SortProbe`, and **nothing at all here** — so this workload
contains no slot that is cat-2 in one range and cat-1 in another. The mechanism
behind `Arrays.sort(long[])` is not merely absent from the yaml failure; the
*shape it needs* is absent from the code. That is measured, not argued, and it
is not something the page could previously say.

Note the consequence for reading the table: on this workload the defect switch
is **inert**, so it is a positive control for the instrument (established on
`SortProbe` above), not for this run.

## The other instrument: register pressure on demand

`CRATONVM_JIT_LOCAL_REGS=<n>` truncates the GPR pool the allocator may colour
locals into. `x64::LOCAL_REGS` is **7 on Windows** (`R12–R15, RBX, RSI, RDI`)
and **5 on System V**, and that gap is the stated reason several failures in
this family are Linux-only: the smaller pool forces the coalescing the dead mask
exists to handle.

Until now a Windows reproduction attempt could not control the one variable
that matters, so a Windows green had to be written off as vacuous rather than
read as evidence. With the knob it is evidence.

## How to use this on the next OSR miscompile

```bash
CRATONVM_DBG_OSR_SEED_COLLISION=1 <cratonvm> ... 2>&1 | grep '^\[osr-seed-'
```

* any `[osr-seed-collision]` or `[osr-seed-stripped]` line **names the method,
  the entry, and the local** — start there;
* `takeable_entries=0` across the whole run means the filter is broken, not that
  the code is clean;
* add `CRATONVM_JIT_LOCAL_REGS=5` (or lower) to reproduce System V pressure on
  Windows;
* add `CRATONVM_JIT_OSR_STRIP_ALL_HIGH_HALVES=1` to check the detector still
  goes red before trusting a green.

`seed_collisions_at` is a pure function with six unit tests, including the exact
`mixedInsertionSort` slot-reuse shape, so its logic is verified independently of
any workload.
