# Self-hosted GPU CI scaffolding

The repository includes `.github/workflows/gpu-selfhosted.yml`, scheduled for
a self-hosted GPU runner, and `bench-gpu/ci-gate.sh`, which runs the benchmark
suite and checks checksums. Hardware-only tests remain explicitly gated so
ordinary CI and CPU-only development do not require a CUDA device.

No CUDA machine is currently enrolled for the workflow label. That is an
operational deployment task rather than a runtime correctness defect; the
workflow and gate script are ready for an enrolled runner.

See [ci.md](ci.md) for how the workflow is structured and operated.
