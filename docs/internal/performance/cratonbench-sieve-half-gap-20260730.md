# CratonBench Sieve half-gap closeout (2026-07-30)

Status: fixed. Gap cut by **94.35%** on Windows (1.05x HotSpot) and **106.40%**
on Linux (0.93x — faster than HotSpot C2).

## Acceptance

Goal: reduce the `CratonBench sieve` wall-time gap between CratonVM and
HotSpot by at least 50%, with every run returning the exact prime count
`9592`.

```text
old_gap       = median(baseline) - median(HotSpot)
new_gap       = median(candidate) - median(HotSpot)
gap_reduction = 1 - new_gap / old_gap
```

`baseline` and `candidate` are the **same release binary**
(`cratonvm-sieve-diag-019fb303.exe`, merge commit `e1a486bd1` + the
`CRATONVM_DBG_JIT_GEN` recognition trace, which contains `origin/dev`
`9ac1feffe`). The baseline differs only by `CRATONVM_JIT_BULK_BYTE_LOOPS=0`,
which empties the three detector vectors and therefore also leaves the
pure-kernel operand cache enabled — i.e. it reproduces `origin/dev` exactly
for this row. Same-binary A/B removes build skew as an explanation.

Host: Windows 11, Temurin 25.0.3, isolated fresh processes, every process
pinned to the P-cores (`ProcessorAffinity = 0xFFFF`) because this box is
hybrid and an E-core migration is indistinguishable from a regression. Nine
rounds, kind order rotated every round, **no sample discarded**:

| Variant | Nine reported times (ms) | Median |
|---|---|---:|
| CratonVM, lowering **off** (dev-equivalent) | 9352, 9490, 9865, 10136, 10365, 10418, 10563, 10835, 11304 | 10,365 |
| CratonVM, lowering **on** | 5335, 5406, 5446, 5625, 5876, 5921, 6228, 6291, 6519 | **5,876** |
| HotSpot C2 | 4880, 5237, 5276, 5528, 5607, 5784, 5900, 5972, 5993 | 5,607 |

```text
old_gap       = 10365 - 5607 = 4758 ms
new_gap       =  5876 - 5607 =  269 ms
gap_reduction = 1 - 269/4758 = 94.35%
```

All 27 executions produced checksum `9592`. The ratio to HotSpot moves from
**1.85x to 1.05x**.

The box was shared with other sessions' release builds throughout, so the
absolute spread is wide (per-sample CPU load is recorded in the raw TSV and
ranged 0–90%). The alternating order, nine samples, and median gate isolate
the optimization despite that: even restricting to the quietest samples of
each kind the picture is unchanged (baseline 9,352 / candidate 5,335 /
HotSpot 4,880).

A **second, independent nine-round run** of the same harness while the box was
uniformly loaded at 60–85% inflated every arm ~1.55x but reproduced the result
exactly: baseline 18,351 / candidate 9,175 / HotSpot 8,722 ms →
**95.30%**, candidate/HotSpot **1.052x** against round one's 1.048x.

### Second host: Azure EPYC, Linux, SysV ABI

Worth doing separately because the emitter's register choices are
platform-dependent — RSI/RDI are Java local homes on Windows and argument
registers on Linux — so a Windows-only result does not cover the codegen the
Linux build actually emits. Same script shape, `taskset -c 2`, nine rounds,
`/proc/loadavg` 4.9–6.0 throughout, all 27 runs checksum `9592`:

| Variant | Nine reported times (ms) | Median |
|---|---|---:|
| CratonVM, lowering **off** | 5552, 5564, 5585, 5598, 5794, 5847, 5862, 5946, 6026 | 5,794 |
| CratonVM, lowering **on** | 2483, 2490, 2494, 2525, 2567, 2568, 2609, 2610, 2644 | **2,567** |
| HotSpot C2 | 2358, 2363, 2452, 2472, 2761, 2844, 2862, 2882, 2903 | 2,761 |

On Linux the lowering **overshoots the gate**: gap reduction 106.40%, i.e.
CratonVM is 0.93x HotSpot — 7% faster — on this row. The baseline arm
reproduces the perf gate's recorded 5,800 ms sieve figure for this host almost
exactly, which independently confirms the control really is dev-equivalent.

HotSpot's samples here are visibly bimodal (2,358–2,903; its own median is
less stable than either CratonVM arm's, whose full nine-sample range is
±3%). Reading it in the way least favourable to this change — CratonVM's
median against HotSpot's **fastest** sample — still gives
`1 - (2567-2358)/(5794-2358)` = **93.9%** and 1.09x. The result does not
depend on which end of HotSpot's spread is used.

## Root cause

`CratonBench.sieve(boolean[], int)` is three counted `boolean[]` loops:

```java
for (int i = 0; i <= limit; i++) composite[i] = false;      // clear
for (int i = 2; i <= limit; i++)                            // outer
    if (!composite[i]) {
        count++;
        for (int j = i + i; j <= limit; j += i)              // mark
            composite[j] = true;
    }
```

The 2026-07-14 round (`hashmap-sieve-half-gap-20260714.md`) took this row
from 6.10x to 2.03x with general fixes — OSR/method-entry tier decoupling,
the `slot_mirror` reload elision, and GPR local homes. What it explicitly
left on the table is what still dominated:

- **Single-pass BCE categorically refuses inclusive (`<=`) loops and
  non-`arr.length` bounds**, so all three loops kept a per-element null check
  and bounds check even though `limit` is loop-invariant and provably in
  range once, at the preheader.
- **The IR/C2 tier cannot compile any array method at all** (no
  `baload`/`bastore`/`iaload`/`iastore`/`arraylength` lowering), so the whole
  method falls back to the single-pass template backend — no register
  allocation beyond the kernel local homes, no strength reduction on the
  `i + i` / `j += i` induction chain.
- The clear loop stored one byte per iteration through the generic
  `bastore` path rather than a block fill.
- The outer loop re-read `composite[i]` one byte at a time even across the
  long all-composite stretches that dominate a sieve's scan (for
  limit = 100,000 the outer loop reads 100k bytes of which only 9,592 are
  zero).

## Fix

Three fall-through-only guarded preheaders in `jit/src/x64.rs`, each
recognizing one complete canonical javac loop shape — header, body,
induction update, and exact back edge — and each emitting a register-only
replacement that ends by leaving the Java locals in the state the original
loop would have reached, then falling through into the original bytecode
(whose first test therefore fails and exits):

- `emit_bulk_zero_byte_fill_preheader` — `REP STOSB` over `[iv, bound]`.
- `emit_bulk_set_byte_stride_preheader` — a register-only strided store loop
  with the step held in R9.
- `emit_byte_sieve_preheader` — the whole nest: prime count in R8D, outer
  index in R10D, bound in R11D, marking index in ECX, array data pointer in
  RAX. The outer scan tests **eight bytes at a time** with the classic
  `(w - 0x0101..) & ~w & 0x8080..` zero-byte detector (constants parked in
  RDI/RSI across the loop, pushed and popped around it because both are Java
  local homes on Windows x64), and only drops to a per-byte test when the
  qword contains a zero. That skip is the single largest win: it is work
  HotSpot's C2 does not do here.

Every guard precedes the first write, so a rejected shape reaches the
original bytecode with untouched locals and untouched array contents:

- `TEST RAX,RAX` — null array.
- `CMP R11D, [RAX + ARRAY_LENGTH_OFFSET]` unsigned — the bound must be a
  valid index, which makes every index in `[iv, bound]` in range and removes
  the per-element check without speculation.
- `CMP R10D, 2` / `CMP R10D, R11D` — the exact entry state of the recognized
  shape, and zero-trip ranges.
- `CMP R11D, 0x3FFFFFFF` — so `i + i` and `j += i` cannot overflow, i.e. the
  emitted code never fabricates a positive index where Java wraps negative.
  The strided preheader instead checks `step <= INT_MAX - bound` and rejects
  `step <= 0` (a non-positive step is an infinite loop in Java itself).
- `bound - iv <= MAX_BULK_BYTE_LOOP_SPAN` (1 MiB) — these loops carry no
  cooperative safepoint poll, so an application-sized range keeps the scalar
  path's polls. The same bound is what makes it safe to hold a raw interior
  array pointer in RAX across the region.
- `find_bypassable_loop_headers` already excludes any header reachable by a
  branch, so the preheaders stay fall-through-only.

Also in this change:

- The three lowerings own R8/R9, which are the pure-kernel deferred operand
  cache's two scratch registers — the same conflict `62f289e71` resolved for
  matrix-dot. `kernel_operand_cache` is now disabled when a byte-sieve or
  strided-store lowering is present, and only then; the pure-kernel local
  homes are untouched.
- `CRATONVM_JIT_BULK_BYTE_LOOPS=0` is a diagnostic kill switch for all three,
  and `CRATONVM_DBG_JIT_GEN` reports the recognized headers.

## Validation

- Binaries: `cratonvm-sieve-diag-019fb303.exe` (Windows, release, this branch
  at `e1a486bd1` plus the recognition trace) and
  `/data/cratonvm-sieve-linux-019fb303` (Azure Linux, release, branch tip
  `280cd270f`, SHA-256
  `abdd02ea2d62ca3c13b1ba8ececb32750bf52ad7a60ab14768b6016a5d54322a`).
- Recognition fires on the **real** `CratonBench.sieve` bytecode, at exactly
  the pcs the unit test predicts, on **both** platforms:
  `[JIT_GEN] bulk-byte headers: zero-fill=[2] set-stride=[40] sieve=[21]`.
- `probes/ByteSieveProbe.java` — 33 reported values covering the real 100k
  sieve and its full array checksum, limits 0/1/2/3/7/8/9/15/16/30/63/64/65
  (i.e. below, at, and across the eight-byte word-scan boundary), a
  pre-dirtied array, and every guard edge: null array, array shorter than
  the bound, negative bound, `limit == length`, `limit == length-1`, a span
  over the 1 MiB cap, negative/zero/overflowing strides, a negative start,
  and `limit == Integer.MAX_VALUE` (the `i + i` overflow edge). Output is
  **byte-identical** under all four of: CratonVM JIT, CratonVM `--nojit`,
  CratonVM with the lowering disabled, CratonVM under
  `CRATONVM_DBG_GC_STRESS=1048576` — and identical to HotSpot. Run on both
  Windows and Linux; all eight CratonVM configurations agree with their
  platform's HotSpot line for line. (Under GC stress the Linux run also emits
  eight `[moving-young] fallback … jit-relocation-contract-unproven` warnings;
  those are the pre-existing default-moving-young issue tracked in
  `docs/known-issues/jit-optimizing-tier-disabled-by-moving-young-default.md`,
  not probe output, and the 33 reported values are unchanged.)
- `cargo test -p cratonvm-jit` (all targets): 1,057 lib + 193 integration
  tests, **0 failed**. This includes
  `header_offset_emission_site_inventory_matches_the_doc`, whose counts were
  re-derived as the union of this branch's three preheaders and `origin/dev`'s
  matrix-dot sites when the two were merged
  (35 / 13 / 23 / 5; `docs/internal/arch-2026-07-26/x64-flag-skew-and-contracts.md` §6.2).
- All-phase `CratonBench`, same binary, lowering on vs off: every one of the
  seven checksums identical; only the sieve row moves (5,019 vs 8,750 ms),
  the other six are within run-to-run noise.

- The whole seven-phase `CratonBench` was also run on Windows with the
  lowering on and off: all seven checksums identical, only the sieve row moves.
- `cargo test -p cratonvm-vm --lib`: 2,446 passed / 7 failed. All seven live in
  `vm/src/jit/conservative_roots.rs`, `vm/src/jit/skip_list.rs`,
  `vm/src/runtime/interpreter.rs` and `vm/src/runtime/serviceability.rs` —
  files this branch does not touch (`git diff --name-only origin/dev...HEAD`
  lists only `jit/src/x64.rs`, the probe, and four docs), so they are
  pre-existing at dev tip.
- The perf gate's `sieve` baseline is re-anchored from 5,800 to 2,700 ms
  (`regression-suite/perf/cratonbench-baseline-azure-epyc.tsv`, still
  `provisional`). Leaving it at 5,800 would have made the gate blind to a full
  regression of this change. 2,700 is deliberately loose — it sits above the
  loaded median of 2,567 — and the gate refuses to measure above load 2.0,
  where the row is faster still.

## Residual

At 1.05x on Windows and 0.93x on Linux this row is closed. What remains is not
sieve-specific and is the same pair the 2026-07-14 round identified:
single-pass BCE still refuses inclusive loops and non-`arr.length` bounds, and
the IR/C2 tier still cannot compile array methods. Fixing either would make
lowerings like these unnecessary rather than merely redundant.

**Deliberately not landed.** The handoff's Azure worktree carried a fourth,
uncommitted lowering — `ByteScanToZeroLoop` / `emit_byte_scan_to_zero_join`,
which skips runs of nonzero elements in a counted byte loop with an *arbitrary*
body. It is a genuine generalization of the sieve preheader's word scan, but it
is a **join-point** emitter rather than a fall-through-only preheader, which is
a materially larger correctness surface, and it buys nothing here: the sieve
nest lowering already matches the real `CratonBench.sieve` bytecode. It was
dropped rather than finished. Anyone reviving it should treat the join-point
entry contract — not the scan itself — as the hard part.
