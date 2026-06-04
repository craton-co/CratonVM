# Bug: comparison-harness render polish (test-infra/run-vm-comparison.sh)

The cross-VM comparison renders a few false labels. Pure harness/reporting fixes — no VM
changes. Independent of the other `continue_prompt_*` bugs.

## Items
- `junit-help`: render mislabels HotSpot/TornadoVM as **FAIL** even though they print
  `Usage: junit ...` with rc=0. Fix the `good` detection in `run_extras` (the
  `^Usage: junit` grep isn't matching; the line is present in the captured summary).
- `dacapo-avrora`: it's the DaCapo-9.12-on-JDK25 **stderr-digest artifact** — DaCapo
  expects the stderr.log digest to equal `da39a3ee...0709` (SHA-1 of the EMPTY string), but
  JDK 25 writes warnings to stderr, so validation "FAILS" on HotSpot AND TornadoVM too.
  Mark it a known non-signal / drop it from the suite (CratonVM "passing" is luck —
  its stderr happened to be empty).
- `commons-math` cratonvm cell: the harness runs the picocli ConsoleLauncher, which
  silently discovers 0 tests on CratonVM (see `continue_prompt_junit5_discovery.md`).
  Either run the programmatic `LauncherFactory` path, or annotate the cell
  "JUnit5 discovery = 0 tests (known)".

## Files
- `test-infra/run-vm-comparison.sh` (`run_extras`, `render`, `run_commons_math`).
- Optionally `test-infra/bc-suite-3way.sh` (classpath @argfile — overlaps with
  `continue_prompt_bc_suite_artifacts.md`).

## Repro
```
bash test-infra/run-vm-comparison.sh   # SECTIONS / VARIANTS / TIMEOUT / HEAP env knobs
SECTIONS=render bash test-infra/run-vm-comparison.sh   # redraw from last TSVs
```
Memory: `reference_cross_vm_comparison_harness`.
