# Elasticsearch Suite Runner

`run-elasticsearch-suite.ps1` runs compiled Elasticsearch JUnit classes under
CratonVM or HotSpot, one process per test class. It uses the Elasticsearch
fixture's per-module classpath files:

- Elasticsearch checkout: `C:\craton\CratonVM\apps\elasticsearch` by default
- Per-module classpath: `<module>\build\craton-testcp.txt`
- Test launcher: `org.junit.runner.JUnitCore`

Each class gets wall-clock timing and persisted stdout/stderr logs.

## Prerequisites

The Elasticsearch checkout must already be compiled and must have:

- `build\craton-testcp.txt` under each module to run
- compiled test classes under module `build\classes\java\test`
- JDK 25 at `C:\Program Files\Java\jdk-25`, or pass `-JdkHome`

For CratonVM runs, use a uniquely named executable. The runner looks for:

1. `-Exe <path>`
2. `$env:CV_BIN`
3. `$env:CRATONVM_ELASTICSEARCH_EXE`
4. `target\release\cratonvm-elasticsearch-suite.exe`

If only `target\release\cratonvm.exe` exists under the repo root, the runner
copies it to `target\release\cratonvm-elasticsearch-suite.exe` and uses that
copy.

## Basic Commands

Refresh class lists and print the first ten discovered classes:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -RefreshLists -Category all -Start 1 -Count 10 -ListOnly
```

Run the first 50 classes under CratonVM with JIT on:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 1 -Count 50 -Parallel 4 -RunName es-craton-jiton-50
```

Run the first 100 historical non-passing classes with JIT off:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Category others -Start 1 -Count 100 -Jit off -Parallel 4 -RunName es-others-nojit-100
```

Capture a HotSpot baseline for a slice:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm hotspot -Category all -Start 1 -Count 100 -Parallel 4 -RunName hotspot-baseline-100
```

## Parameters

| Parameter | Values | Default | Purpose |
|---|---|---:|---|
| `-Category` | `passed`, `others`, `failed`, `all` | `all` | Class set to run. `failed` is an alias for `others`. |
| `-Jit` | `on`, `off` | `on` | `off` adds CratonVM `--nojit`; HotSpot uses `-Xint`. |
| `-Vm` | `craton`, `hotspot` | `craton` | VM to run. |
| `-Start` | integer >= 1 | `1` | 1-based start index within the selected category. |
| `-Count` | integer >= 0 | `0` | Number of classes to run; `0` means through the end. |
| `-Parallel` | integer >= 1 | `1` | Concurrent class processes per mode. |
| `-TimeoutSec` | integer >= 1 | `120` | Per-class timeout. Timeout status is `HANG`. |
| `-RunName` | string | timestamp | Result directory name. |
| `-ElasticsearchRoot` | path | `C:\craton\CratonVM\apps\elasticsearch` | Elasticsearch checkout to run. |
| `-WorkDir` | path | `apps\elasticsearch-suite-runner\.suite` | Generated lists, results, logs, baselines. |
| `-RefCsv` | path | auto | Reference results for passed/others split. TSV or CSV accepted. |
| `-Exe` | path | auto | Unique CratonVM executable. |
| `-JdkHome` | path | `$env:JAVA_HOME`, then JDK 25 default | JDK for HotSpot and CratonVM `--java-home`. |
| `-MaxHeap` | heap string | `2g` | Heap passed to both VMs. |
| `-Seed` | string | `B17AC9D3E1F2A0C4` | Elasticsearch randomized-test seed. |
| `-CratonArgs` | string array | none | Extra CratonVM CLI arguments. |
| `-SkipNativeFixtureCheck` | switch | off | Bypass the mandatory `libvec.so` ABI gate for narrowly scoped diagnostics only. |
| `-RefreshLists` | switch | off | Rebuild `all-tests.tsv`, `passed.tsv`, `others.tsv`. |
| `-ListOnly` | switch | off | Print selected classes without running. |
| `-AllModes` | switch | off | Run four category/JIT modes concurrently. |

## Native `libvec` fixture gate

Before selecting any test classes, the runner validates the Linux x64
`lib/platform/linux-x64/libvec.so` in the exact `-ElasticsearchRoot` supplied.
It refuses to run when the library is absent, has fewer than 155 `vec_*`
exports, or lacks the `bulk8` symbols required by the checked-out tests. This
prevents a fixture error from being misclassified as a suite-wide CratonVM
failure.

If the fixture is absent or stale, rebuild it from the same Elasticsearch
checkout; do not copy a `libvec.so` from another checkout or cached artifact:

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass `
  -File apps/elasticsearch-suite-runner/prepare-elasticsearch-libvec-fixture.ps1 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch
```

The preparation script builds the library in Elasticsearch's checked-in Docker
cross-toolchain, verifies its exports, and atomically installs it. The runner
uses GNU `nm` on Linux and otherwise reuses the preparation image through
Docker to perform the same preflight check. `-SkipNativeFixtureCheck` is
available only for targeted diagnostics where the native-vector path is known
to be out of scope.

Any `CRATONVM_*` environment variable already set in the shell is inherited by
every CratonVM child process. Example:

```powershell
$env:CRATONVM_LOADER_AWARE_RESOLUTION = '1'
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Category others -Start 1 -Count 50 -Jit on
```

## Passed vs Others

`passed` means the class had `status=PASS`, `state=PASS`, `verdict=OK`, or
`cv_status=OK` in a reference result file. Every other discovered class is placed
in `others`, including `FAIL`, `CRASH`, `HANG`, `NOCP`, and classes absent from
the reference.

Reference file lookup order:

1. `-RefCsv <path>`
2. `.suite\reference.tsv`
3. `.suite\reference.csv`
4. `apps\elasticsearch\cratonvm-suite\results.jit.all.tsv`
5. `apps\elasticsearch\cratonvm-suite\results.jit-on.tsv`
6. `apps\elasticsearch\cratonvm-suite\results.nojit.all.tsv`
7. `apps\elasticsearch\cratonvm-suite\results.nojit.tsv`

## Four Concurrent Modes

`-AllModes` launches four background PowerShell child processes and waits for
all of them:

| Mode directory | Category | JIT |
|---|---|---|
| `passed-jit` | `passed` | on |
| `passed-nojit` | `passed` | off |
| `others-jit` | `others` | on |
| `others-nojit` | `others` | off |

The same `-Start` and `-Count` are applied independently to the `passed` and
`others` lists.

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -AllModes -Start 1 -Count 50 -Parallel 2 -RunName four-modes-50
```

## Output Layout

Each mode writes:

```text
.suite\results\<RunName>\<ModeName>\
  results.tsv
  summary.md
  logs\
    <module>.<class>.out.log
    <module>.<class>.err.log
```

`results.tsv` columns:

```text
index, module, class, vm, jit, rc, status, seconds, tests, failed,
stdoutLog, stderrLog, note
```

Long module/class names are truncated in log filenames and get a stable hash
suffix. The full class name is always preserved in `results.tsv`.

Status values:

- `PASS`: JUnitCore exited 0.
- `FAIL`: JUnitCore exited 1 or reported test failures.
- `HANG`: the process exceeded `-TimeoutSec` and was killed.
- `CRASH`: fatal signal, panic, access violation, or non-JUnit process exit.
- `NOCP`: module classpath file was missing.
- `NOSUMMARY`: no JUnit signal and no crash fingerprint.

Runs are resumable. Existing rows in `results.tsv` are skipped when rerunning
the same `-RunName` and mode.

## HotSpot Baseline Files

Every `-Vm hotspot` run writes normal mode results and also copies the baseline
to:

```text
.suite\baseline\hotspot-baseline-<RunName>.tsv
.suite\baseline\hotspot-baseline-<RunName>.md
.suite\baseline\hotspot-baseline-latest.tsv
.suite\baseline\hotspot-baseline-latest.md
```

Use the baseline TSV as a timing/correctness reference when comparing CratonVM
JIT-on and JIT-off runs.
