# `--nojit` type-check and throwable rows are 7–19% slower than on wave 42

**Status: open — filed 2026-09-30 by the orchestrator of interpreter round i1,
after waves 43–46, from fat-LTO benchmarks on the Linux host.** The waves
were landed with this page open, by the user's instruction to finish the
round. No wrong answer is involved. Performance only, `--nojit` (the JIT rows
did not move beyond the host's noise).

## Measurements

All numbers are fat-LTO binaries, `--nojit`, run interleaved on cores 5 and 3,
three rounds each, compared by `bsum39.py`. The host's floor between runs of
one binary is 8–40% on layout-sensitive rows (see the round's status page);
these steps repeated across three separate runs.

Binaries: `lto-w42f` (wave 42 as landed), `lto-w43b`, `lto-w44` (`69568bea6`),
`lto-w45` (`55834015b`), `lto-w46b` (`7524ce9ef`, the wave-46 merge with its
first follow-ups).

| Row | w43 vs w42 | w44 vs w42 | w45 vs w44 | w46 vs w45 | w46 vs w42 (two runs) |
|---|---|---|---|---|---|
| `TypeCheckBench` `classMono` | +2.8% | +2.8% | −5.6% | +15.2% | +15.2% / +17.9% |
| `TypeCheckBench` `classPoly` | +5.9% | +2.8% | −2.0% | +18.8% | +18.5% / +20.8% |
| `TypeCheckBench` `ifacePoly` | +4.4% | +3.0% | −0.3% | +16.0% | +13.1% / +14.1% |
| `TypeCheckBench` `arrNeg` | +4.0% | +4.1% | −0.1% | +10.8% | +13.0% / +14.5% |
| `L7W27DispatchLayoutBench` `castCall` | +1.6% | +0.9% | +0.7% | +12.7% | +11.7% / +13.8% |
| `L7W34ThrowableCostBench` `throw-d2` | +0.3% | −0.1% | +6.6% | +7.2% | +13.0% / +9.9% |
| `L7W34ThrowableCostBench` `trace-d20` | +1.9% | −1.2% | +7.9% | +6.0% | +13.6% / +13.3% |
| `InvokeDoorCostBench` `super-call` | +4.5% | −2.5% | +0.7% | +7.7% | +27.1% / +10.0% |
| `InvokeDoorCostBench` `empty-virtual` | +13.2% | +12.1% | −0.7% | −6.6% | +2.8% / +5.5% |

Against wave 23, wave 46 is still faster on most rows: `ctor` −35%, array
elements −23%, frameless constructors −25% to −34%. The type-check rows are
back where wave 23 had them, within ±4%.

Logs on the host: `/data/wt-interp-w22-logs/bench-w46b/`, `loc46/`, `loc45/`,
`perf46/`.

## What the profile shows

`sudo perf record -F 2000` of `TypeCheckBench`, `lto-w44` against `lto-w46b`,
per symbol. Three functions show up as their own symbols on wave 46 and not
on wave 44, where they were presumably inlined:

* `HeapBitmap::contains` at 2.6%;
* `LocalKey<[Cell<(u64, …)>]>` at 1.6%;
* `LocalKey<Cell<bool>>::with::<enforce_single_os_thread>` at 0.8%
  (`types/src/value.rs`, `#[inline(always)]` closure, `ObjectRef` construction).

`execute_frame_from_index` falls from 48% to 41%, and `op_instanceof` rises
from 3.7% to 4.9%. Waves 45–46 changed no code in the `gc` or `types` crates,
and every new check on the resolution path returns at once while unarmed
(`note_class_resolution`, the initiating record). The first reading is
therefore an LTO inlining and layout shift caused by growth elsewhere in the
`vm` crate, not new work per instruction. That is not proven.

## Next steps

1. Bisect wave 46's five lane merges (`0fb260090` L2, `68f3a0a64` L4,
   `f3a3de226` L5, `621bbce0e` L3, `9106a67a0` L1) with fat-LTO builds on
   `TypeCheckBench` `classMono` / `classPoly` and `castCall`.
2. At the step that moves them, diff `perf report --sort symbol` between the
   two builds and `perf annotate` `op_instanceof`. Look for the callers of
   `HeapBitmap::contains` and `ObjectRef::new` that stopped inlining.
3. If it is inlining, try marking the hot-path helpers the profile names
   `#[inline]` (or the cold new code `#[cold]` / `#[inline(never)]`) and
   re-measure. If it is not, find the new work.
4. Wave 45's throwable step (+3–8%) goes with lane L3's reflective-frame
   listing in published traces (`i45 L3: published traces and natively
   raised throwables list the reflective call's JDK frames`), which runs per
   frame in `fillInStackTrace`; measure it with that code bypassed.
