# Keycloak Suite Runner

`run-keycloak-suite.ps1` runs the compiled Keycloak JUnit suite under CratonVM
or HotSpot, one process per test class. It uses the existing Keycloak harness:

- Keycloak checkout: `apps\keycloak`
- JUnit Platform runner: `apps\keycloak\kc-runner\KcRunner.class`
- Universal classpath: `apps\keycloak\kc-universal-cp.txt`

Every class gets wall-clock timing and persisted stdout/stderr logs.

## Prerequisites

The Keycloak checkout must already be compiled and must have:

- `apps\keycloak\kc-runner\KcRunner.class`
- `apps\keycloak\kc-universal-cp.txt`
- one or more `target\test-classes` directories under `apps\keycloak`
- JDK 25 at `C:\Program Files\Java\jdk-25`, or pass `-JdkHome`

For CratonVM runs, use a uniquely named executable. The runner looks for:

1. `-Exe <path>`
2. `$env:CV_BIN`
3. `$env:CRATONVM_KEYCLOAK_EXE`
4. `target\release\cratonvm-keycloak-suite.exe`

If only `target\release\cratonvm.exe` exists under the repo root, the runner
copies it to `target\release\cratonvm-keycloak-suite.exe` and uses that copy.
This avoids accidental cleanup of unrelated `cratonvm.exe` processes in other
sessions.

## Basic Commands

Refresh class lists and show the first five discovered classes:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 `
  -RefreshLists -Category all -Start 1 -Count 5 -ListOnly
```

Capture a full HotSpot baseline:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 `
  -Vm hotspot -Category all -RunName hotspot-baseline-20260701 -Parallel 1
```

Run the full CratonVM suite with JIT on:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 `
  -Vm craton -Category all -Jit on -RunName craton-jiton-20260701 -Parallel 1
```

Run the first 100 previously passing classes with JIT off:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 `
  -Category passed -Start 1 -Count 100 -Jit off -RunName pass-nojit-100
```

Run classes 501 through 1000 from the non-passing set:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 `
  -Category others -Start 501 -Count 500 -Jit on -RunName others-501-1000
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
| `-TimeoutSec` | integer >= 1 | `600` | Per-class timeout. Timeout status is `HANG`. |
| `-RunName` | string | timestamp | Result directory name. |
| `-KeycloakRoot` | path | sibling `apps\keycloak` | Keycloak checkout to run. |
| `-WorkDir` | path | `apps\keycloak-suite-runner\.suite` | Generated lists, results, logs, baselines. |
| `-RefCsv` | path | auto | Reference results for passed/others split. TSV or CSV accepted. |
| `-Exe` | path | auto | Unique CratonVM executable. |
| `-JdkHome` | path | `$env:JAVA_HOME`, then JDK 25 default | JDK for HotSpot and CratonVM `--java-home`. |
| `-MaxHeap` | heap string | `2g` | Heap passed to both VMs. |
| `-CratonArgs` | string array | none | Extra CratonVM CLI arguments. |
| `-RefreshLists` | switch | off | Rebuild `all-tests.tsv`, `passed.tsv`, `others.tsv`. |
| `-ListOnly` | switch | off | Print selected classes without running. |
| `-AllModes` | switch | off | Run four category/JIT modes concurrently. |

Any `CRATONVM_*` environment variable already set in the shell is inherited by
every CratonVM child process. Example:

```powershell
$env:CRATONVM_XT_JIT_ROOT_SCAN = '1'
$env:CRATONVM_LOADER_AWARE_RESOLUTION = '1'
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 `
  -Category others -Start 1 -Count 100 -Jit on
```

## Passed vs Others

`passed` means the class had `status=PASS` or `state=PASS` in a CratonVM
reference result file. Every other discovered class is placed in `others`,
including `FAIL`, `CRASH`, `HANG`, `LOADFAIL`, `EMPTY`, and classes absent from
the reference.

Reference file lookup order:

1. `-RefCsv <path>`
2. `.suite\reference.tsv`
3. `.suite\reference.csv`
4. `apps\keycloak\kcfull-results-rerun\results.tsv`
5. `apps\keycloak\kcfull-results\results.tsv`

Rebuild the split explicitly:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 `
  -RefreshLists -Category passed -ListOnly
```

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
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 `
  -AllModes -Start 1 -Count 100 -Parallel 1 -RunName four-modes-100
```

Results are written under:

```text
apps\keycloak-suite-runner\.suite\results\four-modes-100\
  passed-jit\
  passed-nojit\
  others-jit\
  others-nojit\
  passed-jit.console.log
  passed-nojit.console.log
  others-jit.console.log
  others-nojit.console.log
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
aborted, skipped, containersFailed, stdoutLog, stderrLog, note
```

Status values:

- `PASS`: KcRunner reported tests and zero failures/container failures.
- `FAIL`: KcRunner reported failed/aborted tests or failed containers.
- `EMPTY`: KcRunner completed but found zero tests.
- `LOADFAIL`: class loading failed before execution.
- `HANG`: the process exceeded `-TimeoutSec` and was killed by PID.
- `CRASH`: crash/panic/fatal signal fingerprint, or process died without a summary.
- `NOSUMMARY`: no KcRunner summary and no crash fingerprint.

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
