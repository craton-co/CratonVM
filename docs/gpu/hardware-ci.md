# Self-hosted GPU CI scaffolding

The repository includes `.github/workflows/gpu-selfhosted.yml`, scheduled for
a self-hosted GPU runner, and `bench-gpu/ci-gate.sh`, which runs the benchmark
suite and checks checksums. Hardware-only tests remain explicitly gated so
ordinary CI and CPU-only development do not require a CUDA device.

No CUDA machine is currently enrolled for the workflow label. That is an
operational deployment task rather than a runtime correctness defect; the
workflow and gate script are ready for an enrolled runner.

**If you have a device in front of you, [hardware-bring-up.md](../known-issues/gpu/hardware-bring-up.md) is the page to work from** — it puts enrolment last,
deliberately. The value is in the first manual gate run, and doing that
first means a red first CI run reads as "the runner is misconfigured"
rather than "something is broken and we don't know where". It also lists
what only hardware can settle, of which the submission drain's device half
is the one nothing has run since the day it was fixed.

See [ci.md](ci.md) for how the workflow is structured and operated.
