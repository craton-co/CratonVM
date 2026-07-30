# CratonBench Sieve half-gap closeout (2026-07-30)

Status: fixed. Gap cut by **94.35%** — the row is now within 4.8% of HotSpot.

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

- Binary: `cratonvm-sieve-diag-019fb303.exe`, release, built from this
  branch at `e1a486bd1` plus the recognition trace.
- Recognition fires on the **real** `CratonBench.sieve` bytecode, at exactly
  the pcs the unit test predicts:
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
  `CRATONVM_DBG_GC_STRESS=1048576` — and identical to HotSpot.
- `cargo test -p cratonvm-jit` (all targets): 1,057 lib + 193 integration
  tests, **0 failed**. This includes
  `header_offset_emission_site_inventory_matches_the_doc`, whose counts were
  re-derived as the union of this branch's three preheaders and `origin/dev`'s
  matrix-dot sites when the two were merged
  (35 / 13 / 23 / 5; `docs/internal/arch-2026-07-26/x64-flag-skew-and-contracts.md` §6.2).
- All-phase `CratonBench`, same binary, lowering on vs off: every one of the
  seven checksums identical; only the sieve row moves (5,019 vs 8,750 ms),
  the other six are within run-to-run noise.

## Residual

At 1.05x this row is effectively closed. What remains is not sieve-specific
and is the same pair the 2026-07-14 round identified: single-pass BCE still
refuses inclusive loops and non-`arr.length` bounds, and the IR/C2 tier still
cannot compile array methods. Fixing either would make lowerings like these
unnecessary rather than merely redundant.
