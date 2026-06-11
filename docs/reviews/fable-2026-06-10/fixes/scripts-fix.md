# scripts-fix — Scripts hardening (Fable review, 2026-06-10)

## Finding
The script collection (`scripts/` + three `test-infra/*.sh`) carried four
portability/personal-info defects flagged in `scripts-review.md`:

- **B2** — three `test-infra/*.sh` ran `taskkill //F //IM cratonvm.exe`, which
  force-kills *every* `cratonvm.exe` on the machine by image name (clobbers
  parallel suites / other worktrees / a dev session) and exits non-zero on a
  no-match (the documented "rc=1/empty-output" footgun).
- **B3** — pervasive hardcoded `ROOT=C:/craton/CratonVM`, stale ephemeral
  worktree roots (`C:/craton/CratonVM/.claude/worktrees/...`,
  `C:/Projects/CratonVM/.claude/worktrees/...`), dead `C:/Projects/cratonvm`
  roots, and `C:/Program Files/Java/jdk-25` (plus an Eclipse-Adoptium JDK)
  baked across the bench/run scripts and the H2/decchurn `.bat` files.
- **B4** — `scripts/build-devverify.bat` hardcoded a personal
  `C:\Users\Victor\.rustup\...\hmrustc.exe` / `hmcargo.exe` custom toolchain.
- **Stale README** — `scripts/README.md` documented `build-h2.bat`,
  `build-rwd.bat`, `build-wt.bat` that do not exist in `scripts/`.

## Root cause
The scripts were authored as a single-machine developer toolbox; absolute
paths and the author's toolchain were inlined, and the README drifted from the
actual file set as build scripts were renamed.

## Exact change

### 1. PID/path-scoped kill (B2) — three `test-infra/*.sh`
Replaced each `taskkill //F //IM cratonvm.exe 2>/dev/null || true` with a
`kill_stray_cratonvm` helper that, via PowerShell `Get-CimInstance
Win32_Process`, kills **only** `cratonvm.exe` whose `ExecutablePath` is under
`$ROOT` (so a parallel suite / another worktree / a dev session survives), and
can never surface a no-match as a failure (`Stop-Process
-ErrorAction SilentlyContinue` + `|| true`). These calls are pre-run stray
cleanups (no PID exists yet to capture), so path-scoping to `$ROOT` is the
correct narrowing.

### 2. Genericized paths (B3)
- `ROOT="${ROOT:-$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null || echo C:/craton/CratonVM)}"`
  in all three `test-infra/*.sh` and in `scripts/`:
  `bench-4way.sh`, `bench-poly-4way.sh`, `bench-appsuite.sh`,
  `run-all-apps-cuda.sh`, `loop-run-all.sh`, `loop-run-seq.sh`,
  `real-run-all.sh`, `run-kc-tests.sh` (the latter previously had no `ROOT`).
  This also corrects the scripts that pointed `ROOT` at an *ephemeral worktree*
  or a *non-existent* `C:/Projects/cratonvm` root.
- `JDK="${JDK:-${JAVA_HOME:-C:/Program Files/Java/jdk-25}}"` (or
  `JH=`/`HOTSPOT="$JDK/bin/java.exe"`) replaced every hardcoded JDK literal,
  including the two Eclipse-Adoptium roots in `loop-run-*`/`real-run-all`.
  Inline `--java-home "C:/Program Files/Java/jdk-25"` literals in the bench
  scripts now use `"$JDK"`. `app-checker.sh` JDK default now `${JAVA_HOME:-...}`.
- `APPS`/`BENCH_CLASSES`/`TORNADO_SDK_SETUP` now derive from `$ROOT` (or are
  env-overridable). `RJVM` in `run-kc-tests.sh` defaults to
  `$ROOT/target/release/cratonvm.exe` (was a sibling worktree), overridable.
- `.bat` files: `run-h2-nojit.bat`, `run-h2-testall.bat`, `run-one-test.bat`,
  `run_decchurn_ab.bat` now derive `REPO` from `%~dp0..`, honour `%JAVA_HOME%`,
  and take a `%CV%` override (defaulting to the same internal worktree/target
  the script used before, marked internal-dev). The build `.bat` files
  (`build-cpu*.bat`, `build-gpu*.bat`, `build-debug.bat`) gained
  `cd /d "%~dp0.."` so relative `--target-dir` lands at the repo root, plus an
  `INTERNAL-DEV-ONLY` header noting the irreducibly machine-specific MSVC
  `vcvars64.bat` / Git-usr-bin PATH.

### 3. Personal toolchain removed (B4) — `scripts/build-devverify.bat`
Dropped the `C:\Users\Victor\...\hmrustc.exe` / `hmcargo.exe` hardcode. Now
uses `cargo` on PATH by default with `if not defined CARGO set "CARGO=cargo"`,
and a header documenting that a custom toolchain can be supplied via the
`CARGO`/`RUSTC` environment variables.

### 4. README reconciled — `scripts/README.md`
Build-Scripts section now lists exactly the 7 `build-*.bat` files that exist
(`build-cpu`, `build-cpu-isolated`, `build-cpu-java`, `build-debug`,
`build-devverify`, `build-gpu`, `build-gpu-isolated`); removed the phantom
`build-h2`/`build-rwd`/`build-wt` entries; added a note that the repo root also
carries `build-cpu.bat` / `build-cpu-rwd.bat`.

## Files touched
- `test-infra/run-all-apps-suites.sh`
- `test-infra/run-comparison-full.sh`
- `test-infra/run-cpu-rerun.sh`
- `scripts/bench-4way.sh`, `scripts/bench-poly-4way.sh`, `scripts/bench-appsuite.sh`
- `scripts/run-all-apps-cuda.sh`, `scripts/loop-run-all.sh`, `scripts/loop-run-seq.sh`,
  `scripts/real-run-all.sh`, `scripts/run-kc-tests.sh`, `scripts/app-checker.sh`
- `scripts/build-devverify.bat`, `scripts/build-cpu.bat`, `scripts/build-cpu-isolated.bat`,
  `scripts/build-cpu-java.bat`, `scripts/build-debug.bat`, `scripts/build-gpu.bat`,
  `scripts/build-gpu-isolated.bat`
- `scripts/run-h2-nojit.bat`, `scripts/run-h2-testall.bat`, `scripts/run-one-test.bat`,
  `scripts/run_decchurn_ab.bat`
- `scripts/README.md`

## Tests added
None (shell/bat infra, no unit-test harness). Verified all 12 edited `.sh`
files pass `bash -n` (syntax), that `ROOT`/`JDK` resolve correctly via
`git rev-parse --show-toplevel` / `$JAVA_HOME`, that the PowerShell
`kill_stray_cratonvm` helper runs cleanly and returns 0 on both match and
no-match, and that the README build entries now match the on-disk file set
1:1 (no `hmrustc`/`hmcargo`/`C:\Users\Victor` references remain).

## Follow-up & risk
- **Risk: low.** Defaults preserve prior behavior on the author's machine; the
  genericization only adds env/`git`-derived fallbacks. The kill helper is
  strictly *narrower* than the old `taskkill /F /IM` (scoped to `$ROOT`), so it
  cannot kill *more* than before. It depends on `powershell.exe` being on PATH
  (true on the Windows/Git-Bash dev environment these scripts target).
- The `.bat` H2/decchurn scripts still default `%CV%` to internal worktree
  builds (`CratonVM-h2val`, `target-h2`); these are now overridable and headed
  internal-dev, but remain dev-loop specific by nature.
- **Out of scope (not touched):** B1 (CI parked under `.github/.wf/`) is owned
  by `ci-activate`; the `fuzz/` stub; TornadoVM SDK absolute paths (inherently
  external-tool-specific, left as overridable/commented).
