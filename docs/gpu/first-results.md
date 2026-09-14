# First real-GPU results

This file was the placeholder for "acceptance numbers once a GPU box exists."
That happened: the offload path was validated end-to-end on an RTX 2060
(sm_75) — device probe, transparent `--gpu` offload, checksum
parity with HotSpot, and head-to-head numbers against TornadoVM 4.0.1 (PTX).

The living results now belong to:

- **`bench-gpu/results/`** — dated benchmark tables (cold 4-way comparison,
  warm-loop comparison, div-chain "GPU vs best CPU" concept-prover).
- **Repo `README.md` → "GPU offload"** — the headline table.
- **[`docs/gpu/reductions.md`](reductions.md)** — intentional CPU fallbacks
  (float reductions) and their rationale.

Acceptance criteria from the original plan, as measured (warm, full
H2D + kernel + D2H per call, `GpuWarm.heavy` n = 2²⁴): correctness ✓
(samples/checksums match HotSpot), speedup ✓ (GPU ≥ 2× target — measured
~475× vs CratonVM CPU JIT and ~1.1–2× vs TornadoVM on the same kernel).
