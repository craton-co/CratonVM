# Problem 7: compiled code spends several times HotSpot's work per operation

Filed 2026-11-12. Updated with Linux `perf stat -e instructions:u` measurements (`--scale full --reps 3`, `(I(3N) - I(N)) / 2N`) across all 20 kernels in `tools/probes/OpCostRows.java` on HotSpot 25 and CratonVM (foundational Real-JDK default and `--compatible` optimization layer) across Serial (`-XX:+UseSerialGC`), G1 (`-XX:+UseG1GC`), and ZGC (`-XX:+UseZGC`).

**Current status:** 83 of 120 `(collector, mode, row)` cells are at or below `2.00x` HotSpot 25 (up from most rows at `3x-307x`), and all 18 parity cells (`allocArr`, `sync`, `throwNew` across 3 collectors and 2 modes) remain well within the `1.05x` baseline ceiling (`0.221x-0.809x`). 37 residual cells across 8 rows remain above `2.00x` and are tracked below.

## Measurement (Linux x64, `instructions:u` per op)

Measured via `python3 scripts/op-cost-budget.py --mode calibrate --scale full --reps 3` (`-Xmx2g`, median of 3 runs per `(I(3N) - I(N)) / 2N`). Full checked-in budgets live in `regression-suite/perf/op-cost-budget.tsv`.

### Serial (`-XX:+UseSerialGC`)

| row | N | HotSpot | Real-JDK | Real ratio | `--compatible` | Compat ratio |
|---|---|---:|---:|---:|---:|---:|
| `arith` | 50,000,000 | 4.34 | 1.34 | 0.31x | 2.74 | 0.63x |
| `getfield` | 50,000,000 | 4.13 | 15.38 | **3.72x** | 15.22 | **3.68x** |
| `putInt` | 50,000,000 | 1.27 | 0.09 | 0.07x | 1.33 | 1.05x |
| `putRef` | 50,000,000 | 10.80 | 30.97 | **2.87x** | 31.54 | **2.92x** |
| `aastore` | 50,000,000 | 10.28 | 26.11 | **2.54x** | 26.12 | **2.54x** |
| `virt2` | 50,000,000 | 13.54 | 26.52 | 1.96x | 26.50 | 1.96x |
| `iface4` | 10,000,000 | 46.09 | 33.06 | 0.72x | 32.93 | 0.71x |
| `lambda` | 50,000,000 | 1.28 | 1.73 | 1.35x | -0.14 | 0.00x |
| `allocObj` | 10,000,000 | 28.04 | 44.70 | 1.59x | 48.19 | 1.72x |
| `allocArr` | 10,000,000 | 30.21 | 45.07 | 1.49x | 44.95 | 1.49x |
| `box` | 50,000,000 | 3.29 | 2.14 | 0.65x | 2.64 | 0.80x |
| `mapGet` | 10,000,000 | 26.04 | 83.68 | **3.21x** | 84.52 | **3.25x** |
| `listGet` | 10,000,000 | 15.82 | 33.94 | **2.15x** | 32.49 | **2.05x** |
| `charAt` | 50,000,000 | 4.37 | 34.06 | **7.79x** | 34.03 | **7.78x** |
| `sbAppend` | 10,000,000 | 16.76 | 30.03 | 1.79x | 30.07 | 1.79x |
| `sync` | 10,000,000 | 12.13 | 21.23 | 1.75x | 21.72 | 1.79x |
| `typeCheck` | 50,000,000 | 14.02 | 34.84 | **2.49x** | 34.55 | **2.46x** |
| `arraycopy` | 10,000,000 | 31.52 | 22.55 | 0.72x | 25.09 | 0.80x |
| `throwCatch` | 10,000,000 | 2.30 | 7.98 | **3.47x** | 6.70 | **2.92x** |
| `throwNew` | 100,000 | 4586.92 | 7882.21 | 1.72x | 7936.15 | 1.73x |

### G1 (`-XX:+UseG1GC`)

| row | N | HotSpot | Real-JDK | Real ratio | `--compatible` | Compat ratio |
|---|---|---:|---:|---:|---:|---:|
| `arith` | 50,000,000 | 4.44 | 1.84 | 0.41x | 1.45 | 0.33x |
| `getfield` | 50,000,000 | 4.92 | 15.48 | **3.15x** | 15.60 | **3.17x** |
| `putInt` | 50,000,000 | 1.31 | 1.43 | 1.09x | 1.52 | 1.16x |
| `putRef` | 50,000,000 | 20.05 | 37.99 | 1.90x | 37.99 | 1.90x |
| `aastore` | 50,000,000 | 16.89 | 41.00 | **2.43x** | 28.87 | 1.71x |
| `virt2` | 50,000,000 | 13.54 | 26.80 | 1.98x | 26.48 | 1.96x |
| `iface4` | 10,000,000 | 46.07 | 33.22 | 0.72x | 33.04 | 0.72x |
| `lambda` | 50,000,000 | 1.28 | 1.21 | 0.95x | 1.38 | 1.08x |
| `allocObj` | 10,000,000 | 36.32 | 62.08 | 1.71x | 60.16 | 1.66x |
| `allocArr` | 10,000,000 | 38.46 | 57.33 | 1.49x | 57.19 | 1.49x |
| `box` | 50,000,000 | 3.32 | 2.06 | 0.62x | 2.08 | 0.63x |
| `mapGet` | 10,000,000 | 26.31 | 83.74 | **3.18x** | 84.05 | **3.19x** |
| `listGet` | 10,000,000 | 15.25 | 36.02 | **2.36x** | 31.72 | **2.08x** |
| `charAt` | 50,000,000 | 4.43 | 34.02 | **7.68x** | 34.04 | **7.68x** |
| `sbAppend` | 10,000,000 | 17.35 | 29.84 | 1.72x | 30.07 | 1.73x |
| `sync` | 10,000,000 | 12.15 | 21.48 | 1.77x | 21.21 | 1.75x |
| `typeCheck` | 50,000,000 | 13.75 | 34.31 | **2.49x** | 34.58 | **2.51x** |
| `arraycopy` | 10,000,000 | 31.62 | 25.00 | 0.79x | 24.08 | 0.76x |
| `throwCatch` | 10,000,000 | 1.70 | 6.59 | **3.88x** | 4.82 | **2.84x** |
| `throwNew` | 100,000 | 4776.13 | 8251.57 | 1.73x | 8049.28 | 1.69x |

### ZGC (`-XX:+UseZGC`)

| row | N | HotSpot | Real-JDK | Real ratio | `--compatible` | Compat ratio |
|---|---|---:|---:|---:|---:|---:|
| `arith` | 50,000,000 | 4.41 | 1.53 | 0.35x | 1.41 | 0.32x |
| `getfield` | 50,000,000 | 6.03 | 19.30 | **3.20x** | 19.27 | **3.20x** |
| `putInt` | 50,000,000 | 1.29 | 1.55 | 1.20x | 1.39 | 1.07x |
| `putRef` | 50,000,000 | 19.54 | 26.96 | 1.38x | 27.03 | 1.38x |
| `aastore` | 50,000,000 | 16.80 | 23.02 | 1.37x | 23.14 | 1.38x |
| `virt2` | 50,000,000 | 15.56 | 26.47 | 1.70x | 26.55 | 1.71x |
| `iface4` | 10,000,000 | 46.80 | 32.85 | 0.70x | 32.58 | 0.70x |
| `lambda` | 50,000,000 | 1.29 | 1.31 | 1.02x | 1.35 | 1.05x |
| `allocObj` | 10,000,000 | 33.01 | 66.86 | **2.03x** | 65.73 | 1.99x |
| `allocArr` | 10,000,000 | 35.51 | 69.65 | 1.96x | 60.61 | 1.71x |
| `box` | 50,000,000 | 3.74 | 5.37 | 1.44x | 1.01 | 0.27x |
| `mapGet` | 10,000,000 | 46.00 | 84.95 | 1.85x | 88.17 | 1.92x |
| `listGet` | 10,000,000 | 16.53 | 38.36 | **2.32x** | 31.40 | 1.90x |
| `charAt` | 50,000,000 | 4.39 | 37.72 | **8.59x** | 39.03 | **8.89x** |
| `sbAppend` | 10,000,000 | 17.99 | 30.92 | 1.72x | 30.56 | 1.70x |
| `sync` | 10,000,000 | 11.86 | 21.00 | 1.77x | 21.16 | 1.78x |
| `typeCheck` | 50,000,000 | 18.51 | 34.56 | 1.87x | 35.10 | 1.90x |
| `arraycopy` | 10,000,000 | 31.32 | 22.52 | 0.72x | 25.13 | 0.80x |
| `throwCatch` | 10,000,000 | 1.75 | 6.17 | **3.53x** | 6.66 | **3.81x** |
| `throwNew` | 100,000 | 4974.82 | 6944.58 | 1.40x | 7038.26 | 1.41x |

### Parity rows vs baseline (`DEV_PARITY_BASELINE_INSNS`)

All 18 parity cells pass (`<= 1.05x` baseline):

- `allocArr`: `45.07 / 91.43 = 0.493x` (Serial Real-JDK), `44.95 / 91.30 = 0.492x` (Serial `--compatible`), `57.33 / 113.33 = 0.506x` (G1 Real-JDK), `57.19 / 112.90 = 0.507x` (G1 `--compatible`), `69.65 / 274.62 = 0.254x` (ZGC Real-JDK), `60.61 / 273.92 = 0.221x` (ZGC `--compatible`).
- `sync`: `21.23 / 89.01 = 0.239x` (Serial Real-JDK), `21.72 / 89.35 = 0.243x` (Serial `--compatible`), `21.48 / 86.27 = 0.249x` (G1 Real-JDK), `21.21 / 86.27 = 0.246x` (G1 `--compatible`), `21.00 / 86.16 = 0.244x` (ZGC Real-JDK), `21.16 / 86.30 = 0.245x` (ZGC `--compatible`).
- `throwNew`: `7882.21 / 11207.89 = 0.703x` (Serial Real-JDK), `7936.15 / 11371.04 = 0.698x` (Serial `--compatible`), `8251.57 / 10193.59 = 0.809x` (G1 Real-JDK), `8049.28 / 10332.66 = 0.779x` (G1 `--compatible`), `6944.58 / 9056.21 = 0.767x` (ZGC Real-JDK), `7038.26 / 9194.27 = 0.766x` (ZGC `--compatible`).

## Completed Architectural Work

1. **Instruction budget gate & `putRef` probe**: Fixed `OpCostRows.putRef` so HotSpot cannot fold the loop away; checked in `scripts/op-cost-budget.py`, `scripts/check-op-cost-budget.sh`, `regression-suite/perf/op-cost-budget.tsv`, and `.github/workflows/op-cost-budget.yml`.
2. **Inlining & JDK bytecode paths**: Virtual/interface polymorphic inline caches and guarded splices (`virt2` `1.70x-1.98x`, `iface4` `0.70x-0.72x`), lambda inlining + loop accumulator retirement (`lambda` `0.95x-1.35x`), `StringBuilder.append` (`1.70x-1.79x`), `HashMap.get` (`zgc` `1.85x-1.92x`), and `ArrayList.get` (`zgc` `--compatible` `1.90x`).
3. **Loop baseline & register residency**: Loop-carried GP register residency, frame-slot clear hoisting, single-instruction page-poll safepoints, fused `And`-mask index loads (`gp_load_index_rcx`), multi-BCI dead-add/pad rematerialization, and trap-free pre-header hoisting (`arith` `0.31x-0.63x`, `putInt` `0.07x-1.20x`).
4. **GC barriers & TLAB allocation**: Whole-heap biased card base for Serial `GC_FLAG_OLD_GEN`, G1 young-screen shift/region-alignment geometry and single `arena_base` load, ZGC 8B-aligned TLAB cursor emission + `bts` start-bit lowering, and `Integer.valueOf`/iota vectorization (`allocObj` `1.59x-2.03x`, `allocArr` `1.49x-1.96x`, `box` `0.27x-1.44x`).
5. **Object checks, exceptions & arraycopy**: Deferred `getfield` implicit null-check page-poll recovery entries, layout-epoch pre-header hoisting + dominator cover, final-class `Op::CheckCast` to `Guard(ExactClassIs)` strengthening, intra-body `throwCatch` branch folding, and inline small `arraycopy` (`0.72x-0.80x`).

## Remaining Residuals (`> 2.00x` HotSpot, 37/120 cells)

1. **`charAt` (`7.68x-8.89x`, 6 cells)**: Per-iteration compact-string `coder == 0` / bounds / `byte[]` load sequence remains uncollapsed inside the tight masked loop compared to HotSpot's range-check elimination and hoisted `byte[]` base.
2. **`getfield` (`3.15x-3.72x`, 6 cells) & `putRef` Serial (`2.87x-2.92x`, 2 cells)**: Pointer-chasing `getfield` still pays extra per-iteration layout/epoch or load/rejoin work relative to HotSpot's single `mov` + implicit NPE; Serial `putRef` pays extra per-store barrier instructions relative to HotSpot's 2-instruction card mark.
3. **`throwCatch` (`2.84x-3.88x`, 6 cells)**: Intra-body throw-to-catch still executes more per-iteration instructions than HotSpot's minimal conditional jump.
4. **`mapGet` Serial/G1 (`3.18x-3.25x`, 4 cells), `listGet` (`2.05x-2.36x`, 5 cells), `typeCheck` Serial/G1 (`2.46x-2.51x`, 4 cells)**: Unhoisted type/null/bounds checks and multi-step `HashMap`/`ArrayList` lookups push these above 2x on Serial/G1 (`zgc/mapGet`, `zgc/typeCheck`, and `zgc --compatible listGet` already pass).
5. **`aastore` Serial (`2.54x`, 2 cells) & G1 Real-JDK (`2.43x`, 1 cell), `zgc/allocObj` Real-JDK (`2.03x`, 1 cell)**: `aastore` needs tighter inline type/card-mark sequences on Serial and G1 Real-JDK; `zgc/allocObj` Real-JDK (`66.86` vs `33.01`) misses `2.00x` by 0.86 instructions/op.

## Latest residual-fix attempt (not yet checked into the budget table)

A further round of general fixes landed (`w7_trap_free_preheader`, `j27_page_poll`, and `opcost_` (21/21) unit tests pass; `BinTreesClassic 18` passes 6/6 across Serial/G1/ZGC in both Real-JDK and `--compatible`; all 18 parity cells pass `<= 1.05x` `DEV_PARITY_BASELINE_INSNS`). A full `--scale full --reps 3` calibration was run on the release binary; `regression-suite/perf/op-cost-budget.tsv` and the tables above are **not** updated to it, because not all 120 cells are `<= 2.00x` yet.

Landed in this attempt:

- **Page-form safepoint poll with `owed_latch`** (`ir_page_poll.rs`, `getfield_legacy_read.rs`): `shape_admits` now admits back-edge page polls even when `gen_owed_poll_flag_addr != 0`, pairing the 2-instruction page-poll fault site with the `owed_latch` test + `JNZ .slow` instead of falling back to the 8-instruction TLS+byte poll.
- **`ClassInitTest` exit-`If` hoisting** (`w7_trap_free_preheader.rs`): `hoist_invariant_exit_ifs` now hoists pure `ClassInitTest` event-guard `If`s (such as `guard_local_throw_events` in `throwCatch`) even when the branch control does not dominate all back edges.
- **`with_compile_request` `class_is_final_resolver` & CHA leaf exact typechecks** (`compile_optimizing_artifact.rs`, `frozen_pages.rs`, `try_osr.rs`, `ir_tier_attempt.rs`, `fast_barriers.rs`, `ir_ic_cascade_admits_kind.rs`): wired `class_is_final_resolver` into `with_compile_request` and added per-compile CHA leaf tracking (`cratonvm/cha_leaf/` dependencies invalidated on subclass definition in `note_class_defined`).
- **`charAt` unroll & masked-bound exit hoisting** (`unroll_pure_body_enabled.rs`, `w7_trap_free_preheader.rs`): admitted `Op::StrCharDecode` in `deopt_mpu` partial unrolling, and added `hoist_invariant_final_array_loads` (non-OSR `final` ref fields feeding `ArrayLength`) plus `masked_exit_bound` hoisting in `hoist_invariant_exit_ifs`. Across all 3 collectors and both modes, `charAt` improved from `34.02-39.03` insns/op to `24.78-25.47` insns/op (`5.59x-5.79x` HotSpot: Serial `25.44 / 24.78`, G1 `25.47 / 25.02`, ZGC `25.04 / 25.02`).

Measured, still above `2.00x` (77/120 cells `<= 2.00x`, 43/120 `> 2.00x`, Real-JDK / `--compatible`): `charAt` (`25.44/24.78` Serial, `25.47/25.02` G1, `25.04/25.02` ZGC); `getfield` (`15.30/15.35` Serial, `15.43/15.23` G1, `19.36/19.42` ZGC); `putRef` Serial (`29.00/29.02`); `aastore` Serial (`24.03/24.06`), G1 Real-JDK (`41.13`); `mapGet` (`85.39/85.11` Serial, `85.29/84.86` G1, `92.84/93.31` ZGC); `listGet` (`35.18/31.98` Serial, `34.91/32.04` G1, `34.39` ZGC Real-JDK); `typeCheck` Serial/G1 (`33.69/34.20` Serial, `34.07/34.02` G1); `throwCatch` (`7.01/9.85` Serial, `7.91/7.21` G1, `7.80/7.08` ZGC); plus `zgc/allocObj` (`66.92/66.63`), `serial/sync --compatible` (`23.68`), `serial/sbAppend` Real-JDK (`39.32`), and `g1/putInt` Real-JDK (`2.87`).

Next to investigate: why `listGet` (`CheckCast(Integer)`) and `typeCheck` (`InstanceOf(P)`) still took the non-final path (likely `outline_slow` / `try_emit_exact_final_checkcast` or inlined-site interning), why `throwCatch` still executes `~7-10` insns/op, `charAt` register residency / bounds-check folding to reach `<= 8.8`, `getfield` register spills (`sum += p.x; p = p.next`), `aastore` G1 Real-JDK vs `--compatible` (`41.13` vs `29.08`), and `mapGet` (`HashMap.get`).

## Done when

- Every row in `regression-suite/perf/op-cost-budget.tsv` is within `2.0x` of HotSpot on all three collectors and both modes. Then `--require-2x` becomes the default in `scripts/op-cost-budget.py`.
- No parity row (`sync`, `throwNew`, `allocArr`) regresses by more than 5 percent vs `DEV_PARITY_BASELINE_INSNS`.
- **Next aim, not yet pursued: every cell within `1.5x` of HotSpot.** This document stays here (it is not retired to `docs/internal/`) until that aim is met.

## Not measured

- Interpreter cost per bytecode: every row here is compiled code.
- Multi-threaded rows, and collectors other than Serial, G1, and ZGC.
